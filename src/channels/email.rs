use crate::channels::traits::{Channel, ChannelMessage};
use async_trait::async_trait;
use lettre::message::Mailbox;
use lettre::message::MultiPart;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use mailparse::MailAddr;
use mailparse::MailHeaderMap;
use pulldown_cmark::{html, Options, Parser};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const EMAIL_REPLY_META_SEP: &str = "\u{001F}";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct EmailThreadMeta {
    message_id: Option<String>,
    subject: Option<String>,
}

#[derive(Debug, Clone)]
struct InboundEmail {
    uid: String,
    sender: String,
    content: String,
    thread: EmailThreadMeta,
}

#[derive(Clone)]
pub struct EmailChannel {
    imap_host: String,
    imap_port: u16,
    imap_login: String,
    imap_password: String,
    imap_starttls: bool,
    smtp_host: String,
    smtp_port: u16,
    smtp_login: String,
    smtp_password: String,
    smtp_starttls: bool,
    from_address: String,
    inbox_folder: String,
    poll_interval_secs: u64,
    allowed_senders: Vec<String>,
}

impl EmailChannel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        imap_host: String,
        imap_port: u16,
        imap_login: String,
        imap_password: String,
        imap_starttls: bool,
        smtp_host: String,
        smtp_port: u16,
        smtp_login: String,
        smtp_password: String,
        smtp_starttls: bool,
        from_address: String,
        inbox_folder: String,
        poll_interval_secs: u64,
        allowed_senders: Vec<String>,
    ) -> Self {
        Self {
            imap_host,
            imap_port,
            imap_login,
            imap_password,
            imap_starttls,
            smtp_host,
            smtp_port,
            smtp_login,
            smtp_password,
            smtp_starttls,
            from_address,
            inbox_folder,
            poll_interval_secs,
            allowed_senders,
        }
    }

    fn is_sender_allowed(&self, sender: &str) -> bool {
        if self.allowed_senders.iter().any(|u| u == "*") {
            return true;
        }
        self.allowed_senders
            .iter()
            .any(|u| u.eq_ignore_ascii_case(sender))
    }

    fn validate_email_identity(value: &str) -> bool {
        let trimmed = value.trim();
        !trimmed.is_empty()
            && trimmed.contains('@')
            && !trimmed.contains('\n')
            && !trimmed.contains('\r')
    }

    fn parse_sender_from_headers(headers: &[mailparse::MailHeader<'_>]) -> Option<String> {
        let from_header = headers.get_first_header("From")?;
        let addrs = mailparse::addrparse_header(from_header).ok()?;

        for addr in addrs.iter() {
            match addr {
                MailAddr::Single(info) => return Some(info.addr.clone()),
                MailAddr::Group(group) => {
                    if let Some(first) = group.addrs.first() {
                        return Some(first.addr.clone());
                    }
                }
            }
        }
        None
    }

    fn parse_text_body(raw_email: &[u8]) -> Option<String> {
        let parsed = mailparse::parse_mail(raw_email).ok()?;

        if !parsed.subparts.is_empty() {
            for part in &parsed.subparts {
                let ctype = part.ctype.mimetype.to_ascii_lowercase();
                if ctype == "text/plain" {
                    let body = part.get_body().ok()?;
                    let trimmed = body.trim().to_string();
                    if !trimmed.is_empty() {
                        return Some(trimmed);
                    }
                }
            }
        }

        let body = parsed.get_body().ok()?;
        let trimmed = body.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    }

    fn encode_thread_meta(meta: &EmailThreadMeta) -> Option<String> {
        if meta.message_id.is_none() && meta.subject.is_none() {
            return None;
        }

        serde_json::to_string(meta).ok()
    }

    fn decode_thread_meta(raw: &str) -> Option<EmailThreadMeta> {
        serde_json::from_str::<EmailThreadMeta>(raw).ok()
    }

    fn parse_recipient_and_thread_meta(recipient: &str) -> (&str, Option<EmailThreadMeta>) {
        if let Some((email, raw_meta)) = recipient.split_once(EMAIL_REPLY_META_SEP) {
            if let Some(meta) = Self::decode_thread_meta(raw_meta) {
                return (email, Some(meta));
            }

            // Backward/compat path: payload may be "<uid><SEP><json-meta>".
            if let Some((_, json_meta)) = raw_meta.split_once(EMAIL_REPLY_META_SEP) {
                return (email, Self::decode_thread_meta(json_meta));
            }

            return (email, None);
        }

        (recipient, None)
    }

    fn reply_subject(subject: Option<&str>) -> String {
        let Some(subject) = subject.map(str::trim).filter(|s| !s.is_empty()) else {
            return "ZeroClaw reply".to_string();
        };

        if subject.to_ascii_lowercase().starts_with("re:") {
            subject.to_string()
        } else {
            format!("Re: {subject}")
        }
    }

    fn markdown_to_html(markdown: &str) -> String {
        let mut options = Options::empty();
        options.insert(Options::ENABLE_TABLES);
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_TASKLISTS);

        let parser = Parser::new_ext(markdown, options);
        let mut html_out = String::new();
        html::push_html(&mut html_out, parser);
        html_out
    }

    fn poll_unseen_blocking(&self) -> anyhow::Result<Vec<InboundEmail>> {
        if !self.imap_starttls {
            anyhow::bail!("imap_starttls=false is not supported in this build");
        }

        let tls = native_tls::TlsConnector::builder().build()?;
        let client = imap::connect(
            (self.imap_host.as_str(), self.imap_port),
            self.imap_host.as_str(),
            &tls,
        )?;
        let mut session = client
            .login(&self.imap_login, &self.imap_password)
            .map_err(|(err, _)| anyhow::anyhow!("IMAP login failed: {err}"))?;

        session.select(&self.inbox_folder)?;
        let unseen = session.search("UNSEEN")?;

        let mut out: Vec<InboundEmail> = Vec::new();
        for uid in unseen {
            let seq = uid.to_string();
            let fetches = session.fetch(seq.as_str(), "RFC822")?;

            for fetch in &fetches {
                let Some(raw_email) = fetch.body() else {
                    continue;
                };

                let Ok(parsed) = mailparse::parse_mail(raw_email) else {
                    continue;
                };

                let Some(sender) = Self::parse_sender_from_headers(&parsed.headers) else {
                    continue;
                };

                let Some(content) = Self::parse_text_body(raw_email) else {
                    continue;
                };

                let thread = EmailThreadMeta {
                    message_id: parsed.headers.get_first_value("Message-ID"),
                    subject: parsed.headers.get_first_value("Subject"),
                };

                out.push(InboundEmail {
                    uid: uid.to_string(),
                    sender,
                    content,
                    thread,
                });
            }

            let _ = session.store(seq.as_str(), "+FLAGS (\\Seen)");
        }

        let _ = session.logout();
        Ok(out)
    }

    fn build_smtp_transport(&self) -> anyhow::Result<AsyncSmtpTransport<Tokio1Executor>> {
        let creds = Credentials::new(self.smtp_login.clone(), self.smtp_password.clone());

        let mut transport_builder = if self.smtp_starttls {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&self.smtp_host)?
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&self.smtp_host)?
        };

        transport_builder = transport_builder.port(self.smtp_port).credentials(creds);
        Ok(transport_builder.build())
    }

    async fn check_imap_connectivity(&self) -> bool {
        let this = self.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            if !this.imap_starttls {
                anyhow::bail!("imap_starttls=false is not supported in this build");
            }

            let tls = native_tls::TlsConnector::builder().build()?;
            let client = imap::connect(
                (this.imap_host.as_str(), this.imap_port),
                this.imap_host.as_str(),
                &tls,
            )?;
            let mut session = client
                .login(&this.imap_login, &this.imap_password)
                .map_err(|(err, _)| anyhow::anyhow!("IMAP login failed: {err}"))?;
            session.select(&this.inbox_folder)?;
            let _ = session.logout();
            Ok(())
        })
        .await
        .ok()
        .and_then(Result::ok)
        .is_some()
    }
}

#[async_trait]
impl Channel for EmailChannel {
    fn name(&self) -> &str {
        "email"
    }

    async fn send(&self, message: &str, recipient: &str) -> anyhow::Result<()> {
        let (recipient_email, thread_meta) = Self::parse_recipient_and_thread_meta(recipient);

        if !Self::validate_email_identity(&self.from_address) {
            anyhow::bail!("Invalid from_address for email channel");
        }
        if !Self::validate_email_identity(recipient_email) {
            anyhow::bail!("Invalid email recipient");
        }

        let from: Mailbox = self.from_address.parse()?;
        let to: Mailbox = recipient_email.parse()?;
        let subject = Self::reply_subject(thread_meta.as_ref().and_then(|m| m.subject.as_deref()));

        let mut builder = Message::builder().from(from).to(to).subject(subject);

        if let Some(message_id) = thread_meta
            .as_ref()
            .and_then(|m| m.message_id.as_ref())
            .map(|m| m.trim())
            .filter(|m| !m.is_empty())
        {
            builder = builder.in_reply_to(message_id.to_string());
            builder = builder.references(message_id.to_string());
        }

        let html_body = Self::markdown_to_html(message);
        let email = builder.multipart(MultiPart::alternative_plain_html(
            message.to_string(),
            html_body,
        ))?;

        let transport = self.build_smtp_transport()?;
        transport.send(email).await?;
        Ok(())
    }

    async fn listen(&self, tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        tracing::info!("Email channel listening on folder {}...", self.inbox_folder);

        let poll_every = std::time::Duration::from_secs(self.poll_interval_secs.max(5));
        let mut seen_ids = HashSet::new();

        loop {
            let this = self.clone();
            let result = tokio::task::spawn_blocking(move || this.poll_unseen_blocking()).await;

            match result {
                Ok(Ok(messages)) => {
                    for inbound in messages {
                        let mut id = inbound.uid.clone();
                        if let Some(meta) = Self::encode_thread_meta(&inbound.thread) {
                            id = format!("{}{}{}", inbound.uid, EMAIL_REPLY_META_SEP, meta);
                        }

                        if seen_ids.contains(&id) {
                            continue;
                        }
                        seen_ids.insert(id.clone());

                        if !self.is_sender_allowed(&inbound.sender) {
                            tracing::warn!(
                                "Email: ignoring message from unauthorized sender: {}",
                                inbound.sender
                            );
                            continue;
                        }

                        let channel_msg = ChannelMessage {
                            id,
                            sender: inbound.sender,
                            content: inbound.content,
                            channel: "email".to_string(),
                            timestamp: std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs(),
                        };

                        if tx.send(channel_msg).await.is_err() {
                            return Ok(());
                        }
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!("Email poll error: {e}");
                }
                Err(e) => {
                    tracing::warn!("Email poll task join error: {e}");
                }
            }

            tokio::time::sleep(poll_every).await;
        }
    }

    async fn health_check(&self) -> bool {
        if !Self::validate_email_identity(&self.from_address) {
            return false;
        }

        if !self.check_imap_connectivity().await {
            return false;
        }

        let Ok(transport) = self.build_smtp_transport() else {
            return false;
        };

        transport.test_connection().await.unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_channel(allowed_senders: Vec<String>) -> EmailChannel {
        EmailChannel::new(
            "imap.example.com".into(),
            993,
            "imap-user".into(),
            "imap-pass".into(),
            true,
            "smtp.example.com".into(),
            587,
            "smtp-user".into(),
            "smtp-pass".into(),
            true,
            "bot@example.com".into(),
            "INBOX".into(),
            10,
            allowed_senders,
        )
    }

    #[test]
    fn email_channel_name() {
        let ch = make_channel(vec![]);
        assert_eq!(ch.name(), "email");
    }

    #[test]
    fn wildcard_sender_allowed() {
        let ch = make_channel(vec!["*".into()]);
        assert!(ch.is_sender_allowed("alice@example.com"));
    }

    #[test]
    fn specific_sender_allowed() {
        let ch = make_channel(vec!["alice@example.com".into()]);
        assert!(ch.is_sender_allowed("alice@example.com"));
        assert!(!ch.is_sender_allowed("bob@example.com"));
    }

    #[test]
    fn sender_allowlist_case_insensitive() {
        let ch = make_channel(vec!["Alice@Example.com".into()]);
        assert!(ch.is_sender_allowed("alice@example.com"));
    }

    #[test]
    fn empty_allowlist_denies_all() {
        let ch = make_channel(vec![]);
        assert!(!ch.is_sender_allowed("alice@example.com"));
    }

    #[test]
    fn validate_email_identity_checks_basic_shape() {
        assert!(EmailChannel::validate_email_identity("alice@example.com"));
        assert!(!EmailChannel::validate_email_identity("aliceexample.com"));
        assert!(!EmailChannel::validate_email_identity(
            "alice@example.com\r\nBcc:x"
        ));
    }

    #[test]
    fn parse_sender_from_header_handles_name_addr() {
        let raw = b"From: Alice Example <alice@example.com>\r\n\r\nhi";
        let parsed = mailparse::parse_mail(raw).expect("parse");
        let sender = EmailChannel::parse_sender_from_headers(&parsed.headers);
        assert_eq!(sender.as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn parse_text_body_prefers_text_plain() {
        let raw = b"From: a@example.com\r\nTo: b@example.com\r\nSubject: T\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nhello world\r\n";
        let body = EmailChannel::parse_text_body(raw);
        assert_eq!(body.as_deref(), Some("hello world"));
    }

    #[test]
    fn reply_subject_prefixes_re_when_needed() {
        assert_eq!(EmailChannel::reply_subject(Some("Hello")), "Re: Hello");
        assert_eq!(EmailChannel::reply_subject(Some("Re: Hello")), "Re: Hello");
    }

    #[test]
    fn recipient_meta_roundtrip() {
        let meta = EmailThreadMeta {
            message_id: Some("<id@example.com>".into()),
            subject: Some("Question".into()),
        };
        let encoded = EmailChannel::encode_thread_meta(&meta).expect("encode");
        let recipient = format!("alice@example.com{}{}", EMAIL_REPLY_META_SEP, encoded);

        let (email, parsed_meta) = EmailChannel::parse_recipient_and_thread_meta(&recipient);
        assert_eq!(email, "alice@example.com");
        let parsed = parsed_meta.expect("meta");
        assert_eq!(parsed.message_id.as_deref(), Some("<id@example.com>"));
        assert_eq!(parsed.subject.as_deref(), Some("Question"));
    }

    #[test]
    fn recipient_meta_roundtrip_with_uid_prefix() {
        let meta = EmailThreadMeta {
            message_id: Some("<id@example.com>".into()),
            subject: Some("Politica Americana".into()),
        };
        let encoded = EmailChannel::encode_thread_meta(&meta).expect("encode");
        let recipient = format!(
            "alice@example.com{}12345{}{}",
            EMAIL_REPLY_META_SEP, EMAIL_REPLY_META_SEP, encoded
        );

        let (email, parsed_meta) = EmailChannel::parse_recipient_and_thread_meta(&recipient);
        assert_eq!(email, "alice@example.com");
        let parsed = parsed_meta.expect("meta");
        assert_eq!(parsed.message_id.as_deref(), Some("<id@example.com>"));
        assert_eq!(parsed.subject.as_deref(), Some("Politica Americana"));
    }

    #[test]
    fn markdown_to_html_renders_headers_and_bold() {
        let html = EmailChannel::markdown_to_html("# Titulo\n\n**negrito**");
        assert!(html.contains("<h1>Titulo</h1>"));
        assert!(html.contains("<strong>negrito</strong>"));
    }
}

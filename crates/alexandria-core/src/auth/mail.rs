//! Outbound mail (issue #102).
//!
//! Delivery is an external service that is not yet integrated with this API.
//! What lives here is the port every operation that needs to send a message
//! depends on, plus the one implementation wired today —
//! [`UnconfiguredMailSender`], which never sends and says so.
//!
//! The point of shipping the port before the provider is that every caller-
//! visible outcome is real now: the front-end can call confirm, resend, and
//! both reset halves, and can handle "the message could not be sent" as a
//! distinct outcome rather than discovering it later. When the external
//! service is integrated it becomes a second implementation behind this same
//! trait, and no handler changes.

use crate::errors::DomainError;

/// A message the core wants delivered to the owner's address.
///
/// Deliberately not a rendered e-mail: the subject and body a provider sends
/// are its concern, including their localization, and the core has no business
/// hard-coding English into a message an owner reads. What the core knows is
/// *which* message this is and the one value it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundMail {
    pub to: String,
    pub kind: MailKind,
    /// The code or token the message must carry. Never logged.
    pub secret: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailKind {
    /// The confirmation code sent at registration and on resend.
    EmailConfirmation,
    /// The reset token sent when a password reset is requested.
    PasswordReset,
}

/// The reason code a caller sees when a send fails. Named here rather than at
/// each call site so both surfaces and every handler report the same string.
pub const MAIL_NOT_CONFIGURED: &str = "mail_not_configured";

/// Outbound mail port. One method: the core hands over a message and learns
/// whether it went.
#[allow(async_fn_in_trait)]
pub trait MailSender: Send + Sync {
    async fn send(&self, message: OutboundMail) -> Result<(), DomainError>;

    /// Whether this transport can send at all, asked without a message.
    ///
    /// Exists so an operation can settle "can anything be delivered here"
    /// *before* it branches on anything address-specific. The password-reset
    /// request is the one that needs it: it deliberately answers the same for
    /// a registered and an unregistered address, and without this check the
    /// registered one would attempt a send and fail while the other returned
    /// early and succeeded — handing back exactly the yes/no the uniform
    /// answer exists to withhold.
    fn available(&self) -> Result<(), DomainError>;
}

/// The implementation wired today: nothing is delivered, and every caller is
/// told so in a form it can act on.
///
/// A failure rather than a silent success. A send that reports success and
/// delivers nothing would leave an owner waiting for a message that is never
/// coming, with the API insisting it was sent — the one outcome worse than an
/// honest refusal.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnconfiguredMailSender;

impl MailSender for UnconfiguredMailSender {
    async fn send(&self, _message: OutboundMail) -> Result<(), DomainError> {
        Err(unconfigured())
    }

    fn available(&self) -> Result<(), DomainError> {
        Err(unconfigured())
    }
}

fn unconfigured() -> DomainError {
    DomainError::unavailable(
        MAIL_NOT_CONFIGURED,
        "outbound mail is not configured, so no message was sent",
    )
}

/// The `MailSender` actually wired at runtime (services.rs), selected once at
/// startup from `MailSettings.provider`.
///
/// An enum rather than a boxed trait object for the same reason
/// `RuntimeAuthService` is one: the set of providers is closed and known at
/// compile time, and `async fn` in a trait is not object-safe here anyway.
/// Today it has exactly one variant; the external service adds the second.
#[derive(Debug, Clone, Copy)]
pub enum RuntimeMailSender {
    Unconfigured(UnconfiguredMailSender),
}

impl MailSender for RuntimeMailSender {
    async fn send(&self, message: OutboundMail) -> Result<(), DomainError> {
        match self {
            RuntimeMailSender::Unconfigured(sender) => sender.send(message).await,
        }
    }

    fn available(&self) -> Result<(), DomainError> {
        match self {
            RuntimeMailSender::Unconfigured(sender) => sender.available(),
        }
    }
}

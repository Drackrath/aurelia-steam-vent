use crate::auth::{ConfirmationError, ConfirmationMethod, RefreshToken, RefreshTokenError};
use crate::connection::ConnectionImpl;
use crate::connection::raw::RawConnection;
use crate::eresult::EResult;
use crate::net::NetworkError;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use steam_vent_core::{JobId, NetMessageHeader, RawSteamId, ReceivableMessage};
use steam_vent_crypto::CryptError;
use steam_vent_proto_common::protobuf::MessageField;
use steam_vent_proto_steam::steammessages_base::CMsgIPAddress;
use steam_vent_proto_steam::steammessages_base::cmsg_ipaddress;
use steam_vent_proto_steam::steammessages_clientserver_login::{
    CMsgClientHello, CMsgClientLogon, CMsgClientLogonResponse,
};
use steamid_ng::{
    AccountType, Instance, InstanceFlags, InstanceType, SteamID, SteamIDParseError, Universe,
};
use thiserror::Error;
use tracing::debug;

type Result<T, E = ConnectionError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConnectionError {
    #[error("Network error: {0:#}")]
    Network(#[from] NetworkError),
    #[error("Login failed: {0:#}")]
    LoginError(#[from] LoginError),
    #[error("Aborted")]
    Aborted,
    #[error("Unsupported confirmation action")]
    UnsupportedConfirmationAction(Vec<ConfirmationMethod>),
}

impl From<ConfirmationError> for ConnectionError {
    fn from(value: ConfirmationError) -> Self {
        match value {
            ConfirmationError::Network(err) => err.into(),
            ConfirmationError::Aborted => ConnectionError::Aborted,
        }
    }
}

impl From<RefreshTokenError> for ConnectionError {
    fn from(value: RefreshTokenError) -> Self {
        ConnectionError::LoginError(value.into())
    }
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LoginError {
    #[error("Access token error: {0:#}")]
    AccessToken(#[from] RefreshTokenError),
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("unknown error {0:?}")]
    Unknown(EResult),
    #[error("steam guard required")]
    SteamGuardRequired,
    #[error("steam returned an invalid public key: {0:#}")]
    InvalidPubKey(CryptError),
    #[error("account not available")]
    UnavailableAccount,
    #[error("rate limited")]
    RateLimited,
    #[error("invalid steam id")]
    InvalidSteamId,
    #[error("steam didn't return a refresh token")]
    NoToken,
}

impl From<EResult> for LoginError {
    fn from(value: EResult) -> Self {
        match value {
            EResult::InvalidPassword => LoginError::InvalidCredentials,
            EResult::AccountDisabled
            | EResult::AccountLockedDown
            | EResult::AccountHasBeenDeleted
            | EResult::AccountNotFound => LoginError::InvalidCredentials,
            EResult::RateLimitExceeded
            | EResult::AccountActivityLimitExceeded
            | EResult::LimitExceeded
            | EResult::AccountLimitExceeded => LoginError::RateLimited,
            EResult::AccountLoginDeniedNeedTwoFactor => LoginError::SteamGuardRequired,
            EResult::InvalidSteamID => LoginError::InvalidSteamId,
            value => LoginError::Unknown(value),
        }
    }
}

impl From<SteamIDParseError> for LoginError {
    fn from(_: SteamIDParseError) -> Self {
        LoginError::InvalidSteamId
    }
}

#[derive(Debug, Clone)]
pub struct JobIdCounter(Arc<AtomicU64>);

impl JobIdCounter {
    #[allow(clippy::should_implement_trait)]
    pub fn next(&self) -> JobId {
        JobId::new(self.0.fetch_add(1, Ordering::Relaxed))
    }
}

impl Default for JobIdCounter {
    fn default() -> Self {
        Self(Arc::new(AtomicU64::new(1)))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Session {
    pub session: RawSession,
    pub auth: SessionAuthenticationDetails,
}

/// A session that might not have authenticated yet
#[derive(Debug, Clone)]
pub(crate) struct RawSession {
    pub session_id: i32,
    pub cell_id: u32,
    pub job_id: JobIdCounter,
    pub heartbeat_interval: Duration,
}

/// Details for authenticated sessions
#[derive(Debug, Clone)]
pub(crate) struct SessionAuthenticationDetails {
    pub public_ip: IpAddr,
    pub ip_country_code: String,
    pub steam_id: SteamID,
    pub app_id: Option<u32>,
    pub refresh_token: RefreshToken,
    /// Steam clock minus ours, seconds.
    pub server_time_offset: i64,
}

/// Unix seconds now, local clock.
pub(crate) fn local_unix_time() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Offset from a logon timestamp.
pub(crate) fn server_time_offset(server_time: u32, local: i64) -> i64 {
    if server_time == 0 {
        return 0;
    }
    i64::from(server_time) - local
}

impl Default for RawSession {
    fn default() -> Self {
        RawSession {
            session_id: 0,
            cell_id: 0,
            job_id: JobIdCounter::default(),
            heartbeat_interval: Duration::from_secs(15),
        }
    }
}

impl SessionAuthenticationDetails {
    pub fn is_server(&self) -> bool {
        self.steam_id.account_type() == AccountType::AnonGameServer
            || self.steam_id.account_type() == AccountType::GameServer
    }

    pub fn with_app_id(mut self, app_id: u32) -> Self {
        self.app_id = Some(app_id);
        self
    }
}

pub async fn anonymous(connection: &RawConnection, account_type: AccountType) -> Result<Session> {
    let mut ip = CMsgIPAddress::new();
    ip.set_v4(0);

    let logon = CMsgClientLogon {
        protocol_version: Some(65580),
        client_os_type: Some(203),
        anon_user_target_account_name: Some(String::from("anonymous")),
        account_name: Some(String::from("anonymous")),
        supports_rate_limit_response: Some(false),
        obfuscated_private_ip: MessageField::some(ip),
        client_language: Some(String::new()),
        chat_mode: Some(2),
        client_package_version: Some(1771),
        ..CMsgClientLogon::default()
    };

    let steam_id = SteamID::new(
        0,
        Instance::new(InstanceType::All, InstanceFlags::None),
        account_type,
        Universe::Public,
    );
    // Anonymous logons carry no token.
    send_logon(
        connection,
        logon,
        steam_id,
        RefreshToken::anonymous(steam_id),
    )
    .await
}

pub async fn login(
    connection: &mut RawConnection,
    account: Option<&str>,
    steam_id: SteamID,
    refresh_token: &str,
    logon_id: u32,
) -> Result<Session> {
    let token = RefreshToken::new(refresh_token.to_string())?;
    let logon = logon_message(account, refresh_token, logon_id);
    send_logon(connection, logon, steam_id, token).await
}

/// Build the client logon message.
///
/// Private IP doubles as logon ID.
fn logon_message(account: Option<&str>, refresh_token: &str, logon_id: u32) -> CMsgClientLogon {
    let mut ip = CMsgIPAddress::new();
    ip.set_v4(logon_id);

    CMsgClientLogon {
        protocol_version: Some(65580),
        client_os_type: Some(203),
        account_name: account.map(String::from),
        supports_rate_limit_response: Some(false),
        obfuscated_private_ip: MessageField::some(ip),
        client_language: Some(String::new()),
        machine_name: Some(String::new()),
        steamguard_dont_remember_computer: Some(false),
        chat_mode: Some(2),
        access_token: Some(refresh_token.into()),
        client_package_version: Some(1771),
        ..CMsgClientLogon::default()
    }
}

async fn send_logon(
    connection: &RawConnection,
    logon: CMsgClientLogon,
    steam_id: SteamID,
    refresh_token: RefreshToken,
) -> Result<Session> {
    let header = NetMessageHeader {
        source_job_id: JobId::NONE,
        target_job_id: JobId::NONE,
        steam_id: RawSteamId::new(steam_id.steam64()),
        ..NetMessageHeader::default()
    };

    let filter = connection.filter();
    let fut = filter.one_kind(CMsgClientLogonResponse::KIND);
    connection.raw_send(header, logon).await?;

    debug!("waiting for login response");
    let raw_response = fut.await.map_err(|_| NetworkError::EOF)?;
    let (header, response) = raw_response.into_header_and_message::<CMsgClientLogonResponse>()?;
    EResult::from_result(response.eresult()).map_err(LoginError::from)?;

    let assigned_steam_id = if response.has_client_supplied_steamid() {
        let raw = response.client_supplied_steamid();
        SteamID::try_from(raw).unwrap_or(steam_id)
    } else if header.steam_id != RawSteamId::NONE {
        SteamID::from_steam64(header.steam_id.id()).unwrap_or(steam_id)
    } else {
        steam_id
    };

    debug!(steam_id = %u64::from(assigned_steam_id), "session started");
    Ok(Session {
        session: RawSession {
            session_id: header.session_id,
            cell_id: response.cell_id(),
            job_id: JobIdCounter::default(),
            heartbeat_interval: Duration::from_secs(response.heartbeat_seconds() as u64),
        },
        auth: SessionAuthenticationDetails {
            public_ip: response
                .public_ip
                .ip
                .as_ref()
                .and_then(|ip| match &ip {
                    cmsg_ipaddress::Ip::V4(bits) => Some(IpAddr::V4(Ipv4Addr::from(*bits))),
                    cmsg_ipaddress::Ip::V6(bytes) if bytes.len() == 16 => {
                        let mut bits = [0u8; 16];
                        bits.copy_from_slice(&bytes[..]);
                        Some(IpAddr::V6(Ipv6Addr::from(bits)))
                    }
                    _ => None,
                })
                .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
            ip_country_code: response.ip_country_code.clone().unwrap_or_default(),
            steam_id: assigned_steam_id,
            app_id: None,
            refresh_token,
            server_time_offset: server_time_offset(
                response.rtime32_server_time(),
                local_unix_time(),
            ),
        },
    })
}

pub async fn hello<C: ConnectionImpl>(conn: &mut C) -> std::result::Result<(), NetworkError> {
    const PROTOCOL_VERSION: u32 = 65580;
    let req = CMsgClientHello {
        protocol_version: Some(PROTOCOL_VERSION),
        ..CMsgClientHello::default()
    };

    let header = NetMessageHeader {
        session_id: 0,
        source_job_id: JobId::NONE,
        target_job_id: JobId::NONE,
        steam_id: RawSteamId::NONE,
        ..NetMessageHeader::default()
    };

    conn.raw_send(header, req).await?;
    Ok(())
}

#[cfg(test)]
mod logon_id_tests {
    use super::*;

    #[test]
    fn server_time_offset_from_logon() {
        assert_eq!(server_time_offset(1_800_000_100, 1_800_000_000), 100);
        assert_eq!(server_time_offset(1_799_999_990, 1_800_000_000), -10);
        // Missing field means no correction.
        assert_eq!(server_time_offset(0, 1_800_000_000), 0);
    }

    #[test]
    fn logon_id_is_sent_as_private_ip() {
        let logon = logon_message(Some("user"), "token", 0xDEAD_BEEF);
        assert_eq!(logon.obfuscated_private_ip.v4(), 0xDEAD_BEEF);
    }

    #[test]
    fn default_logon_id_stays_zero() {
        let logon = logon_message(None, "token", 0);
        assert_eq!(logon.obfuscated_private_ip.v4(), 0);
        assert!(logon.account_name.is_none());
        assert_eq!(logon.access_token(), "token");
    }
}

#[cfg(test)]
mod anonymous_tests {
    use super::*;
    use crate::net::RawNetMessage;
    use bytes::BytesMut;
    use futures_util::StreamExt;
    use tokio::sync::mpsc;

    /// Fake CM answering one logon.
    async fn fake_cm() -> RawConnection {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<BytesMut>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<crate::net::Result<BytesMut>>();
        tokio::spawn(async move {
            // Hello, then the logon.
            let _hello = out_rx.recv().await;
            let logon = out_rx.recv().await.expect("client sent a logon");
            let logon = RawNetMessage::read(logon).unwrap();
            assert_eq!(logon.kind, CMsgClientLogon::KIND);
            let mut response = CMsgClientLogonResponse::new();
            response.set_eresult(1);
            let reply = RawNetMessage::from_message(NetMessageHeader::default(), response)
                .unwrap()
                .into_bytes();
            in_tx.send(Ok(reply)).unwrap();
            // Keep the stream open.
            std::future::pending::<()>().await;
        });
        let sender = futures_util::sink::unfold(out_tx, |tx, msg: BytesMut| async move {
            tx.send(msg).map_err(|_| NetworkError::EOF)?;
            Ok::<_, NetworkError>(tx)
        });
        let receiver = tokio_stream::wrappers::UnboundedReceiverStream::new(in_rx).boxed();
        RawConnection::from_sender_receiver(Box::pin(sender), receiver)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn anonymous_logon_needs_no_token() {
        let connection = fake_cm().await;
        let session = anonymous(&connection, AccountType::AnonUser)
            .await
            .expect("anonymous logon must not demand a token");
        assert!(session.auth.refresh_token.is_empty());
        assert_eq!(session.auth.refresh_token.token(), "");
    }

    #[test]
    fn real_logons_still_need_a_valid_token() {
        assert!(RefreshToken::new("not-a-jwt".into()).is_err());
    }
}

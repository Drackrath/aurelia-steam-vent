# Changelog

## 0.6.0 (unreleased)

### Breaking

#### Authentication

- `AuthConfirmationHandler::or` has been removed, use tuples, arrays, vecs or
  slices of confirmation handlers instead.
- `AuthConfirmationHandler::handle_confirmation` now takes `&mut self` instead
  of `self` and returns a boxed future.
- `login` methods now take a `ClientInfo` to allow customizing how the
  steam-vent presents itself to steam.
- Authenticating with an existing refresh token now requires parsing the raw
  token into a `AccessToken` before using it. And the naming has been fixed from
  using "access token" to "refresh token".
- `AuthConfirmationHandler::handle_confirmation` now returns a `Send` future
  and `AuthConfirmationHandler::boxed` returns a
  `Box<dyn AuthConfirmationHandler + Send>`, so password logins can run inside
  `tokio::spawn` on a multi-threaded runtime. Custom handlers whose futures hold
  non-thread-safe values across an `.await` need to be adjusted.

#### Connection

- The lower level function `one_with_header`, `on_with_header`,
  `send_with_kind`, `raw_send` and `raw_send_with_kind` have been removed from
  the `ConnectionTrait` in order to trim the public api.
- Information about the uses for a `Connection` (steam id, public ip, etc) no
  longer returns an `Option`.

#### Messages

- The `EncodableMessage` trait has been split op into `DecodableMessage` and
  `EncodableMessage` traits.
- The `NetMessage` trait has been split op into `SendableMessage` and
  `ReceivableMessage` traits.
- The `ServiceMethodRequest` trait has been split op into `ServiceMethodRequest`
  and `ServiceNotification` traits.

### Changes

- Steam guard confirmation now retries when an incorrect token is provided,
  `AuthConfirmationHandler::handle_confirmation` will be called once each
  attempts.
- The `ConnectionTrait` and related message traits have been moved to
  `steam-vent-core` (and are re-exported from `steam-vent`). This provides a
  smaller api surface for higher level wrappers (e.g. `steam-vent-chat`) to
  allow decoupling their version from `steam-vent` in most cases.
- Logons can now carry a logon ID through `Connection::login_with_logon_id`,
  `Connection::login_with_refresh_token_and_logon_id` and the matching
  `UnAuthenticatedConnection` methods. Steam ends an earlier session
  (`LogonSessionReplaced`) when another session logs on to the same account
  from the same public IP with the same logon ID, so sessions with different
  IDs can stay connected side by side. `login` and `login_with_refresh_token`
  keep sending 0.

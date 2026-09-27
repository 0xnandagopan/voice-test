//! Server-only transport. Never expose this socket or its credentials to clients.
use crate::protocol::{ClientEvent, ProviderEvent};
use futures_util::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::{
    net::TcpStream,
    time::{Duration, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};

#[derive(Debug, Error)]
pub enum ProviderFailure {
    #[error("VOICE_AGENT_API_KEY is missing or invalid")]
    Configuration,
    #[error("voice provider connection failed")]
    Connection,
    #[error("voice provider timeout")]
    Timeout,
    #[error("voice provider returned an invalid event")]
    Protocol,
}
pub struct VoiceClient {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
}
impl VoiceClient {
    pub async fn connect(api_key: &str) -> Result<Self, ProviderFailure> {
        if api_key.trim().is_empty() {
            return Err(ProviderFailure::Configuration);
        }
        let mut request = "wss://agents.assemblyai.com/v1/ws"
            .into_client_request()
            .map_err(|_| ProviderFailure::Configuration)?;
        let mut auth = format!("Bearer {api_key}")
            .parse::<tokio_tungstenite::tungstenite::http::HeaderValue>()
            .map_err(|_| ProviderFailure::Configuration)?;
        auth.set_sensitive(true);
        request.headers_mut().insert("Authorization", auth);
        let (socket, _) = timeout(
            Duration::from_secs(15),
            tokio_tungstenite::connect_async(request),
        )
        .await
        .map_err(|_| ProviderFailure::Timeout)?
        .map_err(|_| ProviderFailure::Connection)?;
        Ok(Self { socket })
    }
    /// Server-side short-lived token with a provider session cap. Tokens stay
    /// internal to this adapter. The relay still enforces cumulative remaining
    /// allowance; the provider token API has a minimum duration of 60 seconds.
    pub async fn connect_bounded(
        api_key: &str,
        maximum_seconds: u32,
    ) -> Result<Self, ProviderFailure> {
        if api_key.trim().is_empty() || !(60..=360).contains(&maximum_seconds) {
            return Err(ProviderFailure::Configuration);
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| ProviderFailure::Configuration)?;
        let response = client
            .get("https://agents.assemblyai.com/v1/token")
            .bearer_auth(api_key)
            .query(&[
                ("expires_in_seconds", 60),
                ("max_session_duration_seconds", maximum_seconds),
            ])
            .send()
            .await
            .map_err(|_| ProviderFailure::Connection)?;
        if !response.status().is_success() {
            return Err(ProviderFailure::Configuration);
        }
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|_| ProviderFailure::Protocol)?;
        let token = body["token"]
            .as_str()
            .filter(|v| !v.is_empty())
            .ok_or(ProviderFailure::Protocol)?;
        let mut url = url::Url::parse("wss://agents.assemblyai.com/v1/ws")
            .map_err(|_| ProviderFailure::Configuration)?;
        url.query_pairs_mut().append_pair("token", token);
        let (socket, _) = timeout(
            Duration::from_secs(15),
            tokio_tungstenite::connect_async(url.as_str()),
        )
        .await
        .map_err(|_| ProviderFailure::Timeout)?
        .map_err(|_| ProviderFailure::Connection)?;
        Ok(Self { socket })
    }
    pub async fn send(&mut self, event: &ClientEvent) -> Result<(), ProviderFailure> {
        let payload = serde_json::to_string(event).map_err(|_| ProviderFailure::Protocol)?;
        timeout(
            Duration::from_secs(5),
            self.socket.send(Message::Text(payload.into())),
        )
        .await
        .map_err(|_| ProviderFailure::Timeout)?
        .map_err(|_| ProviderFailure::Connection)
    }
    pub async fn receive(&mut self) -> Result<Option<ProviderEvent>, ProviderFailure> {
        self.receive_raw()
            .await?
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| ProviderFailure::Protocol)
    }
    /// Server-only diagnostic surface. May contain transcript/configuration secrets;
    /// callers must never forward or log the raw value.
    pub async fn receive_raw(&mut self) -> Result<Option<serde_json::Value>, ProviderFailure> {
        loop {
            match timeout(Duration::from_secs(20), self.socket.next())
                .await
                .map_err(|_| ProviderFailure::Timeout)?
            {
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str(&text)
                        .map(Some)
                        .map_err(|_| ProviderFailure::Protocol);
                }
                Some(Ok(Message::Close(_))) | None => return Ok(None),
                Some(Ok(Message::Ping(data))) => self
                    .socket
                    .send(Message::Pong(data))
                    .await
                    .map_err(|_| ProviderFailure::Connection)?,
                Some(Ok(_)) => {}
                Some(Err(_)) => return Err(ProviderFailure::Connection),
            }
        }
    }
    /// A WebSocket close without session.end requests native-resume grace.
    /// TCP loss is represented by dropping this client instead.
    pub async fn disconnect(mut self) -> Result<(), ProviderFailure> {
        self.socket
            .close(None)
            .await
            .map_err(|_| ProviderFailure::Connection)
    }
    /// Use only for explicit Stop/finish/discard. Accidental disconnect must not end.
    pub async fn end(mut self) -> Result<(), ProviderFailure> {
        self.send(&ClientEvent::End).await?;
        self.socket
            .close(None)
            .await
            .map_err(|_| ProviderFailure::Connection)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn absent_key_does_not_attempt_network() {
        assert!(matches!(
            VoiceClient::connect("").await,
            Err(ProviderFailure::Configuration)
        ));
    }
    #[test]
    fn provider_error_text_is_not_retained() {
        let event: ProviderEvent = serde_json::from_str(
            r#"{"type":"session.error","code":"invalid_config","message":"sensitive content"}"#,
        )
        .unwrap();
        assert!(!format!("{event:?}").contains("sensitive"));
    }
}

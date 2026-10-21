//! Captcha plugin architecture and interactive challenge resolution.
//!
//! Provides challenge definitions (text, simple math, ASCII/image, reCAPTCHA keys),
//! solver abstractions, and coordination via [`CaptchaManager`] and [`InteractiveCaptchaSolver`].

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use url::Url;
use uuid::Uuid;

use super::{PluginKind, PluginMeta};
use crate::contract::Transport;

/// Types of Captcha challenges supported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChallengePayload {
    /// Text prompt or question (e.g. "What is 3 + 4?", text verification code).
    Text { prompt: String },
    /// Base64 encoded image or ASCII art representation for terminal display.
    Image {
        mime_type: String,
        data_base64: String,
        instructions: Option<String>,
    },
    /// Web-based token challenge (e.g. Turnstile, reCAPTCHA, hCaptcha sitekey).
    Token {
        service: String,
        site_key: String,
        page_url: Url,
    },
}

/// A Captcha challenge requesting a solution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptchaChallenge {
    pub id: Uuid,
    pub host: String,
    pub payload: ChallengePayload,
}

impl CaptchaChallenge {
    pub fn new_text(host: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            host: host.into(),
            payload: ChallengePayload::Text {
                prompt: prompt.into(),
            },
        }
    }

    pub fn new_image(
        host: impl Into<String>,
        mime_type: impl Into<String>,
        data_base64: impl Into<String>,
        instructions: Option<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            host: host.into(),
            payload: ChallengePayload::Image {
                mime_type: mime_type.into(),
                data_base64: data_base64.into(),
                instructions,
            },
        }
    }

    pub fn new_token(
        host: impl Into<String>,
        service: impl Into<String>,
        site_key: impl Into<String>,
        page_url: Url,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            host: host.into(),
            payload: ChallengePayload::Token {
                service: service.into(),
                site_key: site_key.into(),
                page_url,
            },
        }
    }
}

/// Response/solution provided for a Captcha challenge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptchaSolution {
    pub challenge_id: Uuid,
    pub text: String,
}

impl CaptchaSolution {
    pub fn new(challenge_id: Uuid, text: impl Into<String>) -> Self {
        Self {
            challenge_id,
            text: text.into(),
        }
    }
}

/// Plugin trait for hoster/service specific Captcha handling.
#[async_trait]
pub trait CaptchaPlugin: Send + Sync {
    fn meta(&self) -> PluginMeta;

    fn can_handle(&self, url: &Url) -> bool {
        let host = url.host_str().unwrap_or("");
        self.meta()
            .domains
            .iter()
            .any(|d| host == d || host.ends_with(&format!(".{d}")))
    }

    /// Fetches a new challenge from the hoster/service using the provided Transport.
    async fn fetch_challenge(
        &self,
        url: &Url,
        transport: &dyn Transport,
    ) -> Result<CaptchaChallenge>;

    /// Validates the challenge solution against the hoster/service.
    async fn verify_solution(
        &self,
        solution: &CaptchaSolution,
        transport: &dyn Transport,
    ) -> Result<bool>;
}

/// Abstract solver interface: could be interactive TUI, automatic OCR, or external solver API.
#[async_trait]
pub trait CaptchaSolver: Send + Sync {
    async fn solve(&self, challenge: &CaptchaChallenge) -> Result<CaptchaSolution>;
}

/// Interactive TUI solver that passes challenges to an interactive user channel.
#[derive(Clone)]
pub struct InteractiveCaptchaSolver {
    prompt_tx: mpsc::UnboundedSender<(CaptchaChallenge, oneshot::Sender<String>)>,
}

impl InteractiveCaptchaSolver {
    pub fn new(
        prompt_tx: mpsc::UnboundedSender<(CaptchaChallenge, oneshot::Sender<String>)>,
    ) -> Self {
        Self { prompt_tx }
    }
}

#[async_trait]
impl CaptchaSolver for InteractiveCaptchaSolver {
    async fn solve(&self, challenge: &CaptchaChallenge) -> Result<CaptchaSolution> {
        let (solution_tx, solution_rx) = oneshot::channel();
        self.prompt_tx
            .send((challenge.clone(), solution_tx))
            .map_err(|_| anyhow::anyhow!("Interactive captcha prompt channel closed"))?;

        let text = solution_rx
            .await
            .context("User cancelled or closed captcha prompt")?;
        Ok(CaptchaSolution::new(challenge.id, text))
    }
}

/// Registry and coordinator for Captcha plugins and resolution.
#[derive(Default)]
pub struct CaptchaManager {
    plugins: HashMap<String, Arc<dyn CaptchaPlugin>>,
}

impl CaptchaManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, plugin: Arc<dyn CaptchaPlugin>) {
        self.plugins.insert(plugin.meta().id, plugin);
    }

    pub fn find_for_url(&self, url: &Url) -> Option<Arc<dyn CaptchaPlugin>> {
        self.plugins.values().find(|p| p.can_handle(url)).cloned()
    }

    /// Resolves a Captcha for a given URL using matching plugin and solver.
    pub async fn resolve_for_url(
        &self,
        url: &Url,
        solver: &dyn CaptchaSolver,
        transport: &dyn Transport,
    ) -> Result<CaptchaSolution> {
        let plugin = self
            .find_for_url(url)
            .ok_or_else(|| anyhow::anyhow!("No captcha plugin found for {url}"))?;

        let challenge = plugin.fetch_challenge(url, transport).await?;
        let solution = solver.solve(&challenge).await?;

        let is_valid = plugin.verify_solution(&solution, transport).await?;
        if !is_valid {
            bail!("Captcha solution was rejected by {}", challenge.host);
        }

        Ok(solution)
    }
}

/// Built-in text/math challenge plugin (useful as mock or for simple verification pages).
pub struct MathCaptchaPlugin {
    domains: Vec<String>,
}

impl MathCaptchaPlugin {
    pub fn new(domains: Vec<String>) -> Self {
        Self { domains }
    }
}

#[async_trait]
impl CaptchaPlugin for MathCaptchaPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            id: "math-captcha".into(),
            name: "Math Captcha Plugin".into(),
            version: "0.1.0".into(),
            author: "rdownloader".into(),
            description: "Simple interactive math challenge solver".into(),
            kind: PluginKind::Captcha,
            domains: self.domains.clone(),
        }
    }

    async fn fetch_challenge(
        &self,
        url: &Url,
        _transport: &dyn Transport,
    ) -> Result<CaptchaChallenge> {
        let host = url.host_str().unwrap_or("unknown");
        Ok(CaptchaChallenge::new_text(host, "Please solve: 12 + 15"))
    }

    async fn verify_solution(
        &self,
        solution: &CaptchaSolution,
        _transport: &dyn Transport,
    ) -> Result<bool> {
        Ok(solution.text.trim() == "27")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::MockTransport;

    #[tokio::test]
    async fn interactive_solver_roundtrip() {
        let (prompt_tx, mut prompt_rx) = mpsc::unbounded_channel();
        let solver = InteractiveCaptchaSolver::new(prompt_tx);

        let challenge = CaptchaChallenge::new_text("test.host", "What is 2 + 2?");
        let challenge_id = challenge.id;

        tokio::spawn(async move {
            let (chal, reply_tx) = prompt_rx.recv().await.expect("receive challenge");
            assert_eq!(chal.id, challenge_id);
            if let ChallengePayload::Text { prompt } = chal.payload {
                assert_eq!(prompt, "What is 2 + 2?");
            } else {
                panic!("Expected text payload");
            }
            reply_tx.send("4".to_string()).expect("send response");
        });

        let solution = solver.solve(&challenge).await.unwrap();
        assert_eq!(solution.challenge_id, challenge_id);
        assert_eq!(solution.text, "4");
    }

    #[tokio::test]
    async fn captcha_manager_resolution_flow() {
        let mut manager = CaptchaManager::new();
        manager.register(Arc::new(MathCaptchaPlugin::new(vec!["example.com".into()])));

        let url = Url::parse("https://example.com/download").unwrap();
        let transport = MockTransport::new();

        let (prompt_tx, mut prompt_rx) = mpsc::unbounded_channel();
        let solver = InteractiveCaptchaSolver::new(prompt_tx);

        // Correct answer branch
        tokio::spawn(async move {
            let (_chal, reply_tx) = prompt_rx.recv().await.expect("receive challenge");
            reply_tx.send("27".to_string()).expect("send solution");
        });

        let result = manager.resolve_for_url(&url, &solver, &transport).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().text, "27");
    }

    #[tokio::test]
    async fn captcha_manager_rejects_wrong_answer() {
        let mut manager = CaptchaManager::new();
        manager.register(Arc::new(MathCaptchaPlugin::new(vec!["example.com".into()])));

        let url = Url::parse("https://example.com/download").unwrap();
        let transport = MockTransport::new();

        let (prompt_tx, mut prompt_rx) = mpsc::unbounded_channel();
        let solver = InteractiveCaptchaSolver::new(prompt_tx);

        tokio::spawn(async move {
            let (_chal, reply_tx) = prompt_rx.recv().await.expect("receive challenge");
            reply_tx.send("99".to_string()).expect("send solution");
        });

        let result = manager.resolve_for_url(&url, &solver, &transport).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("rejected"));
    }
}

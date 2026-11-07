use kurisu_core::{Core, Event, EventKind, TrackingPrompt};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LegacyPrompt {
    #[serde(flatten)]
    pub prompt: TrackingPrompt,
    pub generation: String,
    pub account_id: Option<i64>,
    pub prompt_id: String,
    pub ask: bool,
}

impl LegacyPrompt {
    pub fn from_event(event: &Event) -> Option<Self> {
        let (prompt, ask) = match &event.kind {
            EventKind::TrackingPrompt(prompt) => (prompt, false),
            EventKind::TrackingAsk(prompt) => (prompt, true),
            _ => return None,
        };
        Some(Self {
            prompt: prompt.clone(),
            generation: event.generation.to_string(),
            account_id: event.account_id,
            prompt_id: event.prompt_id?.to_string(),
            ask,
        })
    }

    fn into_event(self) -> Result<Event, String> {
        Ok(Event {
            generation: self
                .generation
                .parse()
                .map_err(|_| "Invalid prompt generation")?,
            account_id: self.account_id,
            prompt_id: Some(
                self.prompt_id
                    .parse()
                    .map_err(|_| "Invalid prompt identity")?,
            ),
            kind: if self.ask {
                EventKind::TrackingAsk(self.prompt)
            } else {
                EventKind::TrackingPrompt(self.prompt)
            },
        })
    }
}

pub async fn acknowledge(core: &Core, event: LegacyPrompt) -> Result<bool, String> {
    Ok(core.acknowledge_tracking_prompt(event.into_event()?).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::TestPlatform;
    use kurisu_core::UpdateChannel;
    use std::sync::Arc;

    #[tokio::test]
    async fn legacy_skip_survives_lag_without_consuming_an_identical_reissue() {
        let path = std::env::temp_dir().join(format!("kurisu-legacy-skip-{}", std::process::id()));
        let core = Core::open(
            path.clone(),
            tokio::runtime::Handle::current(),
            Arc::new(TestPlatform),
            UpdateChannel::Legacy,
        )
        .unwrap();
        let mut receiver = core.subscribe();
        let payload = TrackingPrompt {
            media_id: 1,
            episode: 2,
            progress: 1,
            title: "Synthetic".into(),
            raw_title: "Synthetic 02".into(),
        };
        for ask in [false, true] {
            core.emit(if ask {
                EventKind::TrackingAsk(payload.clone())
            } else {
                EventKind::TrackingPrompt(payload.clone())
            });
            let event = core
                .recovery_events()
                .into_iter()
                .find(|event| event.prompt_id.is_some())
                .unwrap();
            let wire: LegacyPrompt = serde_json::from_value(
                serde_json::to_value(LegacyPrompt::from_event(&event).unwrap()).unwrap(),
            )
            .unwrap();
            assert!(acknowledge(&core, wire.clone()).await.unwrap());
            for id in 0..260 {
                core.emit(EventKind::PendingChanged(Some(vec![id])));
            }
            assert!(matches!(
                receiver.recv().await,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
            ));
            assert!(!core
                .recovery_events()
                .iter()
                .any(|event| event.prompt_id.is_some()));
            core.emit(if ask {
                EventKind::TrackingAsk(payload.clone())
            } else {
                EventKind::TrackingPrompt(payload.clone())
            });
            let current = core
                .recovery_events()
                .into_iter()
                .find(|event| event.prompt_id.is_some())
                .unwrap();
            assert!(!acknowledge(&core, wire).await.unwrap());
            assert!(core.event_is_current(&current));
            let mut wrong = LegacyPrompt::from_event(&current).unwrap();
            wrong.ask = !wrong.ask;
            assert!(!acknowledge(&core, wrong).await.unwrap());
            assert!(
                acknowledge(&core, LegacyPrompt::from_event(&current).unwrap())
                    .await
                    .unwrap()
            );
        }
        drop(receiver);
        drop(core);
        std::fs::remove_dir_all(path).unwrap();
    }
}

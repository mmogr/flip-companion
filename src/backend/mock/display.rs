use crate::platform::display::DisplayDetector;
use crate::types::display::{Geometry, OutputId, OutputInfo};

/// Mock display detector returning a fake dual-screen setup.
pub struct MockDisplayDetector;

impl DisplayDetector for MockDisplayDetector {
    async fn list_outputs(&self) -> anyhow::Result<Vec<OutputInfo>> {
        Ok(vec![
            OutputInfo {
                id: OutputId("MOCK-TOP".into()),
                name: "Mock Top Screen".into(),
                enabled: true,
                geometry: Geometry {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
            },
            OutputInfo {
                id: OutputId("MOCK-BOTTOM".into()),
                name: "Mock Bottom Screen".into(),
                enabled: true,
                geometry: Geometry {
                    x: 0,
                    y: 1080,
                    width: 480,
                    height: 800,
                },
            },
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lists_two_outputs() {
        let detector = MockDisplayDetector;
        let outputs = detector.list_outputs().await.unwrap();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].id, OutputId("MOCK-TOP".into()));
        assert_eq!(outputs[1].id, OutputId("MOCK-BOTTOM".into()));
    }
}

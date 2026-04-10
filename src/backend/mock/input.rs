use async_trait::async_trait;

use crate::platform::input::InputInjector;

/// Mock input injector that logs keystrokes to stdout.
pub struct MockInputInjector;

#[async_trait]
impl InputInjector for MockInputInjector {
    async fn type_text(&self, text: &str) -> anyhow::Result<()> {
        println!("[mock-input] type_text: {text:?}");
        Ok(())
    }

    async fn press_key(&self, key: &str) -> anyhow::Result<()> {
        println!("[mock-input] press_key: {key:?}");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn type_text_succeeds() {
        let injector = MockInputInjector;
        injector.type_text("hello").await.unwrap();
    }

    #[tokio::test]
    async fn press_key_succeeds() {
        let injector = MockInputInjector;
        injector.press_key("Return").await.unwrap();
    }
}

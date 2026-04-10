// Flip Companion — bottom-screen panel for AYANEO Flip DS on Bazzite
//
// Architecture:
//   UI thread  ←→  tokio::sync::mpsc channels  ←→  async backend (tokio)
//   Backend pushes updates to UI via slint::invoke_from_event_loop()
//   UI sends commands to backend via mpsc::Sender
//   Never block the Slint event loop with async calls.

fn main() {
    println!("flip-companion — not yet implemented");
}

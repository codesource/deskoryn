//! `deskorynd input-test` — a standalone input-backend diagnostic.
//!
//! M2's real backends (evdev capture + uinput injection on Linux) can only be
//! validated on hardware with the right device permissions, so they are not
//! covered by the automated test suite. This command exercises them in isolation
//! — no peer, no network — so the user can confirm capture and injection work
//! before relying on a live session:
//!
//! * **capture**: reads (without grabbing, so you keep control of your machine)
//!   and prints every pointer/keyboard event for a few seconds;
//! * **inject** (`--inject`): emits a small cursor wiggle through the virtual
//!   uinput device so you can see injection land locally.
//!
//! With the default (portable) build the backend is `Null` and this command just
//! explains how to get a real one (`--features linux`).

use deskoryn_input::platform::{self, Backend};
use std::time::Duration;

pub async fn input_test(secs: u64, inject: bool) -> anyhow::Result<()> {
    let backend = platform::detect();
    println!("input backend: {backend:?}");

    if backend == Backend::Null {
        println!(
            "this is the portable no-op backend — it never captures or injects.\n\
             rebuild with the OS backend to test real devices, e.g.:\n  \
             cargo run -p deskoryn-daemon --features linux -- input-test"
        );
        return Ok(());
    }

    if inject {
        match platform::open_injector() {
            Ok(mut inj) => {
                println!("injecting a cursor wiggle (watch your pointer)...");
                for _ in 0..20 {
                    inj.inject(deskoryn_core::input::InputEvent::PointerMotion { dx: 8, dy: 0 })
                        .await?;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                for _ in 0..20 {
                    inj.inject(deskoryn_core::input::InputEvent::PointerMotion { dx: -8, dy: 0 })
                        .await?;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                inj.release_all().await?;
                println!("injection ok.");
            }
            Err(e) => println!("injector unavailable: {e}"),
        }
    }

    let mut capture = match platform::open_capture() {
        Ok(c) => c,
        Err(e) => {
            println!("capture unavailable: {e}");
            return Ok(());
        }
    };

    println!("reading input for {secs}s (move the mouse / press keys; not grabbed)...");
    let deadline = tokio::time::sleep(Duration::from_secs(secs));
    tokio::pin!(deadline);
    let mut count = 0u64;
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            ev = capture.next_event() => {
                match ev {
                    Ok(ev) => {
                        count += 1;
                        // Throttle noisy pointer motion in the printout.
                        if !matches!(ev, deskoryn_core::input::InputEvent::PointerMotion { .. }) || count % 20 == 0 {
                            println!("  {ev:?}");
                        }
                    }
                    Err(e) => {
                        println!("capture ended: {e}");
                        break;
                    }
                }
            }
        }
    }
    println!("captured {count} events.");
    Ok(())
}

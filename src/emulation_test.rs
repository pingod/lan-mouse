use crate::config::Config;
use clap::Args;
use input_emulation::{InputEmulation, InputEmulationError};
use input_event::{Event, KeyboardEvent, PointerEvent, scancode};
use std::f64::consts::PI;
use std::time::{Duration, Instant};

const FREQUENCY_HZ: f64 = 1.0;
const RADIUS: f64 = 100.0;

#[derive(Args, Clone, Debug, Eq, PartialEq)]
pub struct TestEmulationArgs {
    #[arg(long)]
    mouse: bool,
    #[arg(long)]
    keyboard: bool,
    #[arg(long)]
    scroll: bool,
}

pub async fn run(config: Config, args: TestEmulationArgs) -> Result<(), InputEmulationError> {
    let TestEmulationArgs {
        mut mouse,
        keyboard,
        scroll,
    } = args;
    /* no flag at all: exercise the pointer, which is what this test was
     * originally written for
     */
    if !mouse && !keyboard && !scroll {
        mouse = true;
    }
    log::info!(
        "running input emulation test (mouse: {mouse}, keyboard: {keyboard}, scroll: {scroll})"
    );

    let backend = config.emulation_backend().map(|b| b.into());
    let mut emulation = InputEmulation::new(backend, config.scroll_smoothing()).await?;
    emulation.create(0).await;

    let start = Instant::now();
    let mut offset = (0, 0);
    /* the key and scroll tests run at a human-readable pace instead of the
     * 1 ms pointer cadence
     */
    let mut last_toggle = Instant::now();
    let mut key_down = false;
    let mut scroll_direction = 1;
    loop {
        tokio::time::sleep(Duration::from_millis(1)).await;

        /* The daemon's event loop drives the smoother; this test has its own
         * loop, so it has to tick it here. */
        if emulation
            .next_scroll_tick()
            .is_some_and(|at| at <= Instant::now())
        {
            emulation.scroll_tick().await?;
        }

        if (keyboard || scroll) && last_toggle.elapsed() >= Duration::from_millis(500) {
            last_toggle = Instant::now();
            if keyboard {
                key_down = !key_down;
                let state = u8::from(key_down);
                let key = scancode::Linux::KeyA as u32;
                log::info!("keyboard test: KeyA state {state}");
                emulation
                    .consume(
                        Event::Keyboard(KeyboardEvent::Key {
                            time: 0,
                            key,
                            state,
                        }),
                        0,
                    )
                    .await?;
            }
            if scroll {
                scroll_direction = -scroll_direction;
                log::info!("scroll test: {scroll_direction} notch(es)");
                emulation
                    .consume(
                        Event::Pointer(PointerEvent::AxisDiscrete120 {
                            axis: 0,
                            value: scroll_direction * 120,
                        }),
                        0,
                    )
                    .await?;
            }
        }

        if !mouse {
            continue;
        }

        let elapsed = start.elapsed();
        let elapsed_sec_f64 = elapsed.as_secs_f64();
        let second_fraction = elapsed_sec_f64 - elapsed_sec_f64 as u64 as f64;
        let radians = second_fraction * 2. * PI * FREQUENCY_HZ;
        let new_offset_f = (radians.cos() * RADIUS * 2., (radians * 2.).sin() * RADIUS);
        let new_offset = (new_offset_f.0 as i32, new_offset_f.1 as i32);
        if new_offset != offset {
            let relative_motion = (new_offset.0 - offset.0, new_offset.1 - offset.1);
            offset = new_offset;
            let (dx, dy) = (relative_motion.0 as f64, relative_motion.1 as f64);
            let event = Event::Pointer(PointerEvent::Motion { time: 0, dx, dy });
            emulation.consume(event, 0).await?;
        }
    }
}

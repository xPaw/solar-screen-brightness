use crate::apply::{apply_brightness, ApplyResults};
use crate::config::SsbConfig;
use human_repr::HumanDuration;
use std::collections::HashSet;
use std::mem::take;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{mpsc, Arc, RwLock};
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use sunrise_sunset_calculator::SunriseSunsetParameters;

pub enum Message {
    Shutdown,
    Refresh(&'static str),
    Disable(&'static str),
    Enable(&'static str),
    Unpause(&'static str),
    Pause(&'static str, i64),
    FullscreenAdd(String),
    FullscreenRemove(String),
}

pub struct BrightnessController {
    pub sender: mpsc::Sender<Message>,
    pub last_result: Arc<RwLock<Option<ApplyResults>>>,
    join_handle: Option<JoinHandle<()>>,
}

impl BrightnessController {
    pub fn start<F: Fn() + Send + 'static>(
        config: Arc<RwLock<SsbConfig>>,
        on_update: F,
    ) -> BrightnessController {
        let (sender, receiver) = mpsc::channel();
        let last_result = Arc::new(RwLock::new(None));
        let cloned = last_result.clone();
        let join_handle = thread::spawn(move || {
            run(config, receiver, cloned, on_update);
        });
        BrightnessController {
            sender,
            last_result,
            join_handle: Some(join_handle),
        }
    }
}

impl Drop for BrightnessController {
    fn drop(&mut self) {
        self.sender.send(Message::Shutdown).unwrap();
        take(&mut self.join_handle).unwrap().join().unwrap();
        log::debug!("Stopped BrightnessController");
    }
}

fn run<F: Fn()>(
    config: Arc<RwLock<SsbConfig>>,
    receiver: mpsc::Receiver<Message>,
    last_result: Arc<RwLock<Option<ApplyResults>>>,
    on_update: F,
) {
    log::info!("Starting BrightnessController");
    let mut enabled = true;
    // When paused, this will be set to the SystemTime until which updates are paused.
    let mut paused_until: Option<SystemTime> = None;
    let mut fullscreen_overrides: HashSet<String> = HashSet::new();

    loop {
        // If we are paused, check whether the pause period has expired.
        if let Some(pause_time) = paused_until {
            if SystemTime::now() >= pause_time {
                log::info!("BrightnessController pause period expired, resuming updates");
                paused_until = None;
            }
        }

        let timeout = if enabled {
            // Apply brightness using latest config
            let config = config.read().unwrap().clone();
            let is_paused = paused_until.is_some();
            let result = apply(config, is_paused, fullscreen_overrides.clone());
            let timeout = calculate_timeout(&result);

            // Update last result
            *last_result.write().unwrap() = result;
            on_update();
            timeout
        } else {
            log::info!("BrightnessController is disabled, skipping update");
            None
        };

        // Sleep until receiving message or timeout
        let rx_result = match timeout {
            None => {
                log::info!("BrightnessController sleeping indefinitely");
                receiver.recv().map_err(|e| e.into())
            }
            Some(timeout) => {
                let duration = timeout
                    .duration_since(SystemTime::now())
                    .unwrap_or_default();
                log::info!(
                    "BrightnessController sleeping for {}s",
                    duration.human_duration()
                );
                receiver.recv_timeout(duration)
            }
        };

        match rx_result {
            Ok(Message::Shutdown) => {
                log::info!("Stopping BrightnessController");
                break;
            }
            Ok(Message::Refresh(src)) => {
                log::info!("Refreshing BrightnessController due to '{src}'");
            }
            Ok(Message::Disable(src)) => {
                log::info!("Disabling BrightnessController due to '{src}'");
                enabled = false;
            }
            Ok(Message::Enable(src)) => {
                log::info!("Enabling BrightnessController due to '{src}'");
                enabled = true;
            }
            Ok(Message::Unpause(src)) => {
                log::info!("Unpausing BrightnessController due to '{src}'");
                paused_until = None;
            }
            Ok(Message::Pause(src, time)) => {
                log::info!("Pausing BrightnessController due to '{src}' for '{time}' seconds");
                let pause_time = if time < 0 {
                    let config = config.read().unwrap().clone();
                    compute_next_sunrise(config)
                } else {
                    SystemTime::now() + Duration::from_secs(time as u64)
                };
                paused_until = Some(pause_time);
            }
            Ok(Message::FullscreenAdd(id)) => {
                log::info!("Adding monitor {id} to fullscreen override");
                fullscreen_overrides.insert(id);
            }
            Ok(Message::FullscreenRemove(id)) => {
                log::info!("Removing monitor {id} from fullscreen override");
                fullscreen_overrides.remove(&id);
            }
            Err(RecvTimeoutError::Timeout) => {
                log::debug!("Refreshing BrightnessController due to timeout")
            }
            Err(RecvTimeoutError::Disconnected) => panic!("Unexpected disconnection"),
        }
    }
}

// The time at which the brightness should be re-applied
fn calculate_timeout(results: &Option<ApplyResults>) -> Option<SystemTime> {
    if let Some(results) = results {
        results
            .monitors
            .iter()
            .flat_map(|m| m.brightness.as_ref().map(|b| b.expiry_time))
            .flatten()
            .min()
            .map(|e| UNIX_EPOCH + Duration::from_secs(e as u64))
    } else {
        None
    }
}

// Calculate and apply the brightness
fn apply(
    config: SsbConfig,
    force_day_brightness: bool,
    fullscreen_overrides: HashSet<String>,
) -> Option<ApplyResults> {
    if let Some(location) = config.location {
        Some(apply_brightness(
            config.brightness_day,
            config.brightness_night,
            config.transition_mins,
            location,
            config.overrides,
            force_day_brightness,
            Some(fullscreen_overrides),
        ))
    } else {
        log::warn!("Skipping apply because no location is configured");
        None
    }
}

// Calculate the next sunrise time
fn compute_next_sunrise(config: SsbConfig) -> SystemTime {
    if let Some(location) = config.location {
        let epoch_time_now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let sun =
            SunriseSunsetParameters::new(epoch_time_now, location.latitude, location.longitude)
                .calculate()
                .unwrap();

        // If the calculated sunrise is in the past, calculate tomorrow's sunrise.
        if sun.rise <= epoch_time_now {
            let tomorrow = epoch_time_now + 86400;
            let sun_tomorrow =
                SunriseSunsetParameters::new(tomorrow, location.latitude, location.longitude)
                    .calculate()
                    .unwrap();
            UNIX_EPOCH + Duration::from_secs(sun_tomorrow.rise as u64)
        } else {
            UNIX_EPOCH + Duration::from_secs(sun.rise as u64)
        }
    } else {
        log::warn!("Assuming next sunrise is in 12 hours because no location is configured");
        SystemTime::now() + Duration::from_secs(43200)
    }
}

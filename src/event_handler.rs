use crate::action::Action;
use crate::client::{build_client, WMClient};
use crate::config::application::Application;
use crate::config::key_press::{KeyPress, Modifier};
use crate::config::keymap::{build_override_table, OverrideEntry};
use crate::config::keymap_action::KeymapAction;
use crate::config::modmap_action::{ModmapAction, MultiPurposeKey, PressReleaseKey};
use crate::config::remap::Remap;
use crate::event::{Event, KeyEvent};
use crate::Config;
use evdev::Key;
use lazy_static::lazy_static;
use log::debug;
use nix::sys::time::TimeSpec;
use nix::sys::timerfd::{Expiration, TimerFd, TimerSetTimeFlags};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::time::{Duration, Instant};

pub struct EventHandler {
    // Currently pressed modifier keys
    modifiers: HashSet<Key>,
    // Modifiers that are currently pressed but not in the source KeyPress
    extra_modifiers: HashSet<Key>,
    // Make sure the original event is released even if remapping changes while holding the key
    pressed_keys: HashMap<Key, Key>,
    // Check the currently active application
    application_client: WMClient,
    application_cache: Option<String>,
    // State machine for multi-purpose keys
    multi_purpose_keys: HashMap<Key, MultiPurposeKeyState>,
    // Current nested remaps
    override_remap: Option<HashMap<Key, Vec<OverrideEntry>>>,
    // Whether to use exact match for override remaps
    override_remap_exact_match: bool,
    // Key triggered on a timeout of nested remaps
    override_timeout_key: Option<Key>,
    // Trigger a timeout of nested remaps through select(2)
    override_timer: TimerFd,
    // { set_mode: String }
    mode: String,
    // { set_mark: true }
    mark_set: bool,
    // { escape_next_key: true }
    escape_next_key: bool,
    // keypress_delay_ms
    keypress_delay: Duration,
    // Buffered actions to be dispatched. TODO: Just return actions from each function instead of using this.
    actions: Vec<Action>,
}

impl EventHandler {
    pub fn new(timer: TimerFd, mode: &str, keypress_delay: Duration) -> EventHandler {
        EventHandler {
            modifiers: HashSet::new(),
            extra_modifiers: HashSet::new(),
            pressed_keys: HashMap::new(),
            application_client: build_client(),
            application_cache: None,
            multi_purpose_keys: HashMap::new(),
            override_remap: None,
            override_remap_exact_match: false,
            override_timeout_key: None,
            override_timer: timer,
            mode: mode.to_string(),
            mark_set: false,
            escape_next_key: false,
            keypress_delay,
            actions: vec![],
        }
    }

    // Handle an Event and return Actions. This should be the only public method of EventHandler.
    pub fn on_event(&mut self, event: &Event, config: &Config) -> Result<Vec<Action>, Box<dyn Error>> {
        match event {
            Event::KeyEvent(key_event) => self.on_key_event(key_event, config),
            Event::OverrideTimeout => self.timeout_override(),
        }?;
        Ok(self.actions.drain(..).collect())
    }

    // Handle EventType::KEY
    fn on_key_event(&mut self, event: &KeyEvent, config: &Config) -> Result<(), Box<dyn Error>> {
        self.application_cache = None; // expire cache
        let key = Key::new(event.code());
        debug!("=> {}: {:?}", event.value(), &key);

        // Apply modmap
        let mut key_values = if let Some(key_action) = self.find_modmap(config, &key) {
            self.dispatch_keys(key_action, key, event.value())?
        } else {
            vec![(key, event.value())]
        };
        self.maintain_pressed_keys(key, event.value(), &mut key_values);
        if !self.multi_purpose_keys.is_empty() {
            key_values = self.flush_timeout_keys(key_values);
        }

        // Apply keymap
        for (key, value) in key_values.into_iter() {
            if config.virtual_modifiers.contains(&key) {
                self.update_modifier(key, value);
                continue;
            } else if MODIFIER_KEYS.contains(&key) {
                self.update_modifier(key, value);
            } else if is_pressed(value) {
                if self.escape_next_key {
                    self.escape_next_key = false
                } else if let Some(actions) = self.find_keymap(config, &key)? {
                    self.dispatch_actions(&actions, &key)?;
                    continue;
                }
            }
            self.send_key(&key, value);
        }
        Ok(())
    }

    fn timeout_override(&mut self) -> Result<(), Box<dyn Error>> {
        if let Some(key) = self.override_timeout_key {
            self.send_key(&key, PRESS);
            self.send_key(&key, RELEASE);
        }
        self.remove_override()
    }

    fn remove_override(&mut self) -> Result<(), Box<dyn Error>> {
        self.override_timer.unset()?;
        self.override_remap = None;
        self.override_timeout_key = None;
        Ok(())
    }

    fn send_keys(&mut self, keys: &Vec<Key>, value: i32) {
        for key in keys {
            self.send_key(key, value);
        }
    }

    fn send_key(&mut self, key: &Key, value: i32) {
        //let event = InputEvent::new(EventType::KEY, key.code(), value);
        let event = KeyEvent::new_with(key.code(), value);
        self.send_action(Action::KeyEvent(event));
    }

    fn send_action(&mut self, action: Action) {
        self.actions.push(action);
    }

    // Repeat/Release what's originally pressed even if remapping changes while holding it
    fn maintain_pressed_keys(&mut self, key: Key, value: i32, events: &mut Vec<(Key, i32)>) {
        // Not handling multi-purpose keysfor now; too complicated
        if events.len() != 1 || value != events[0].1 {
            return;
        }

        let event = events[0];
        if value == PRESS {
            self.pressed_keys.insert(key, event.0);
        } else {
            if let Some(original_key) = self.pressed_keys.get(&key) {
                events[0].0 = *original_key;
            }
            if value == RELEASE {
                self.pressed_keys.remove(&key);
            }
        }
    }

    fn dispatch_keys(
        &mut self,
        key_action: ModmapAction,
        key: Key,
        value: i32,
    ) -> Result<Vec<(Key, i32)>, Box<dyn Error>> {
        let keys = match key_action {
            ModmapAction::Key(modmap_key) => vec![(modmap_key, value)],
            ModmapAction::MultiPurposeKey(MultiPurposeKey {
                held,
                alone,
                alone_timeout,
            }) => {
                if value == PRESS {
                    self.multi_purpose_keys.insert(
                        key,
                        MultiPurposeKeyState {
                            held,
                            alone,
                            alone_timeout_at: Some(Instant::now() + alone_timeout),
                        },
                    );
                    return Ok(vec![]); // delay the press
                } else if value == REPEAT {
                    if let Some(state) = self.multi_purpose_keys.get_mut(&key) {
                        return Ok(state.repeat());
                    }
                } else if value == RELEASE {
                    if let Some(state) = self.multi_purpose_keys.remove(&key) {
                        return Ok(state.release());
                    }
                } else {
                    panic!("unexpected key event value: {}", value);
                }
                // fallthrough on state discrepancy
                vec![(key, value)]
            }
            ModmapAction::PressReleaseKey(PressReleaseKey { press, release }) => {
                // Just hook actions, and then emit the original event. We might want to
                // support reordering the key event and dispatched actions later.
                if value == PRESS {
                    self.dispatch_actions(&press, &key)?;
                }
                if value == RELEASE {
                    self.dispatch_actions(&release, &key)?;
                }
                // Dispatch the original key as well
                vec![(key, value)]
            }
        };
        Ok(keys)
    }

    fn flush_timeout_keys(&mut self, key_values: Vec<(Key, i32)>) -> Vec<(Key, i32)> {
        let mut flush = false;
        for (_, value) in key_values.iter() {
            if *value == PRESS {
                flush = true;
                break;
            }
        }

        if flush {
            let mut flushed: Vec<(Key, i32)> = vec![];
            for (_, state) in self.multi_purpose_keys.iter_mut() {
                flushed.extend(state.force_held());
            }
            flushed.extend(key_values);
            flushed
        } else {
            key_values
        }
    }

    fn find_modmap(&mut self, config: &Config, key: &Key) -> Option<ModmapAction> {
        for modmap in &config.modmap {
            if let Some(key_action) = modmap.remap.get(key) {
                if let Some(application_matcher) = &modmap.application {
                    if !self.match_application(application_matcher) {
                        continue;
                    }
                }
                return Some(key_action.clone());
            }
        }
        None
    }

    fn find_keymap(&mut self, config: &Config, key: &Key) -> Result<Option<Vec<KeymapAction>>, Box<dyn Error>> {
        if let Some(override_remap) = &self.override_remap {
            if let Some(entries) = override_remap.clone().get(key) {
                self.remove_override()?;
                for exact_match in [true, false] {
                    if self.override_remap_exact_match && !exact_match {
                        continue;
                    }
                    for entry in entries {
                        let (extra_modifiers, missing_modifiers) = self.diff_modifiers(&entry.modifiers);
                        if (exact_match && extra_modifiers.len() > 0) || missing_modifiers.len() > 0 {
                            continue;
                        }
                        return Ok(Some(with_extra_modifiers(&entry.actions, &extra_modifiers)));
                    }
                }
            }
            // An override remap is set but not used. Flush the pending key.
            self.timeout_override()?;
        }
        if let Some(entries) = config.keymap_table.get(key) {
            for exact_match in [true, false] {
                for entry in entries {
                    if entry.exact_match && !exact_match {
                        continue;
                    }
                    let (extra_modifiers, missing_modifiers) = self.diff_modifiers(&entry.modifiers);
                    if (exact_match && extra_modifiers.len() > 0) || missing_modifiers.len() > 0 {
                        continue;
                    }
                    if let Some(application_matcher) = &entry.application {
                        if !self.match_application(application_matcher) {
                            continue;
                        }
                    }
                    if let Some(modes) = &entry.mode {
                        if !modes.contains(&self.mode) {
                            continue;
                        }
                    }
                    self.override_remap_exact_match = entry.exact_match;
                    return Ok(Some(with_extra_modifiers(&entry.actions, &extra_modifiers)));
                }
            }
        }
        Ok(None)
    }

    fn dispatch_actions(&mut self, actions: &Vec<KeymapAction>, key: &Key) -> Result<(), Box<dyn Error>> {
        for action in actions {
            self.dispatch_action(action, key)?;
        }
        Ok(())
    }

    fn dispatch_action(&mut self, action: &KeymapAction, key: &Key) -> Result<(), Box<dyn Error>> {
        match action {
            KeymapAction::KeyPress(key_press) => self.send_key_press(key_press),
            KeymapAction::Remap(Remap {
                remap,
                timeout,
                timeout_key,
            }) => {
                self.override_remap = Some(build_override_table(remap));
                if let Some(timeout) = timeout {
                    let expiration = Expiration::OneShot(TimeSpec::from_duration(*timeout));
                    // TODO: Consider handling the timer in ActionDispatcher
                    self.override_timer.unset()?;
                    self.override_timer.set(expiration, TimerSetTimeFlags::empty())?;
                    self.override_timeout_key = timeout_key.or_else(|| Some(*key));
                }
            }
            KeymapAction::Launch(command) => self.run_command(command.clone()),
            KeymapAction::SetMode(mode) => {
                self.mode = mode.clone();
                println!("mode: {}", mode);
            }
            KeymapAction::SetMark(set) => self.mark_set = *set,
            KeymapAction::WithMark(key_press) => self.send_key_press(&self.with_mark(key_press)),
            KeymapAction::EscapeNextKey(escape_next_key) => self.escape_next_key = *escape_next_key,
            KeymapAction::SetExtraModifiers(keys) => {
                self.extra_modifiers.clear();
                for key in keys {
                    self.extra_modifiers.insert(*key);
                }
            }
        }
        Ok(())
    }

    fn send_key_press(&mut self, key_press: &KeyPress) {
        // Build extra or missing modifiers. Note that only MODIFIER_KEYS are handled
        // because logical modifiers shouldn't make an impact outside xremap.
        let (mut extra_modifiers, mut missing_modifiers) = self.diff_modifiers(&key_press.modifiers);
        extra_modifiers.retain(|key| MODIFIER_KEYS.contains(&key) && !self.extra_modifiers.contains(&key));
        missing_modifiers.retain(|key| MODIFIER_KEYS.contains(&key));

        // Emulate the modifiers of KeyPress
        self.send_keys(&missing_modifiers, PRESS);
        self.send_keys(&extra_modifiers, RELEASE);

        // Press the main key
        self.send_key(&key_press.key, PRESS);
        self.send_key(&key_press.key, RELEASE);

        self.send_action(Action::Delay(self.keypress_delay));

        // Resurrect the original modifiers
        self.send_keys(&extra_modifiers, PRESS);
        self.send_keys(&missing_modifiers, RELEASE);
    }

    fn with_mark(&self, key_press: &KeyPress) -> KeyPress {
        if self.mark_set && !self.match_modifier(&Modifier::Shift) {
            let mut modifiers = key_press.modifiers.clone();
            modifiers.push(Modifier::Shift);
            KeyPress {
                key: key_press.key,
                modifiers,
            }
        } else {
            key_press.clone()
        }
    }

    fn run_command(&mut self, command: Vec<String>) {
        self.send_action(Action::Command(command));
    }

    // Return (extra_modifiers, missing_modifiers)
    fn diff_modifiers(&self, modifiers: &Vec<Modifier>) -> (Vec<Key>, Vec<Key>) {
        let extra_modifiers: Vec<Key> = self
            .modifiers
            .iter()
            .filter(|modifier| !contains_modifier(modifiers, modifier))
            .map(|modifier| modifier.clone())
            .collect();
        let missing_modifiers: Vec<Key> = modifiers
            .iter()
            .filter_map(|modifier| {
                if self.match_modifier(modifier) {
                    None
                } else {
                    match modifier {
                        Modifier::Shift => Some(Key::KEY_LEFTSHIFT),
                        Modifier::Control => Some(Key::KEY_LEFTCTRL),
                        Modifier::Alt => Some(Key::KEY_LEFTALT),
                        Modifier::Windows => Some(Key::KEY_LEFTMETA),
                        Modifier::Key(key) => Some(*key),
                    }
                }
            })
            .collect();
        return (extra_modifiers, missing_modifiers);
    }

    fn match_modifier(&self, modifier: &Modifier) -> bool {
        match modifier {
            Modifier::Shift => {
                self.modifiers.contains(&Key::KEY_LEFTSHIFT) || self.modifiers.contains(&Key::KEY_RIGHTSHIFT)
            }
            Modifier::Control => {
                self.modifiers.contains(&Key::KEY_LEFTCTRL) || self.modifiers.contains(&Key::KEY_RIGHTCTRL)
            }
            Modifier::Alt => self.modifiers.contains(&Key::KEY_LEFTALT) || self.modifiers.contains(&Key::KEY_RIGHTALT),
            Modifier::Windows => {
                self.modifiers.contains(&Key::KEY_LEFTMETA) || self.modifiers.contains(&Key::KEY_RIGHTMETA)
            }
            Modifier::Key(key) => self.modifiers.contains(key),
        }
    }

    fn match_application(&mut self, application_matcher: &Application) -> bool {
        // Lazily fill the wm_class cache
        if self.application_cache.is_none() {
            match self.application_client.current_application() {
                Some(application) => self.application_cache = Some(application),
                None => self.application_cache = Some(String::new()),
            }
        }

        if let Some(application) = &self.application_cache {
            if let Some(application_only) = &application_matcher.only {
                return application_only.iter().any(|m| m.matches(application));
            }
            if let Some(application_not) = &application_matcher.not {
                return application_not.iter().all(|m| !m.matches(application));
            }
        }
        false
    }

    fn update_modifier(&mut self, key: Key, value: i32) {
        if value == PRESS {
            self.modifiers.insert(key);
        } else if value == RELEASE {
            self.modifiers.remove(&key);
        }
    }
}

fn with_extra_modifiers(actions: &Vec<KeymapAction>, extra_modifiers: &Vec<Key>) -> Vec<KeymapAction> {
    let mut result: Vec<KeymapAction> = vec![];
    if extra_modifiers.len() > 0 {
        // Virtually release extra modifiers so that they won't be physically released on KeyPress
        result.push(KeymapAction::SetExtraModifiers(extra_modifiers.clone()));
    }
    result.extend(actions.clone());
    if extra_modifiers.len() > 0 {
        // Resurrect the modifier status
        result.push(KeymapAction::SetExtraModifiers(vec![]));
    }
    return result;
}

fn contains_modifier(modifiers: &Vec<Modifier>, key: &Key) -> bool {
    for modifier in modifiers {
        if match modifier {
            Modifier::Shift => key == &Key::KEY_LEFTSHIFT || key == &Key::KEY_RIGHTSHIFT,
            Modifier::Control => key == &Key::KEY_LEFTCTRL || key == &Key::KEY_RIGHTCTRL,
            Modifier::Alt => key == &Key::KEY_LEFTALT || key == &Key::KEY_RIGHTALT,
            Modifier::Windows => key == &Key::KEY_LEFTMETA || key == &Key::KEY_RIGHTMETA,
            Modifier::Key(modifier_key) => key == modifier_key,
        } {
            return true;
        }
    }
    false
}

lazy_static! {
    static ref MODIFIER_KEYS: [Key; 8] = [
        // Shift
        Key::KEY_LEFTSHIFT,
        Key::KEY_RIGHTSHIFT,
        // Control
        Key::KEY_LEFTCTRL,
        Key::KEY_RIGHTCTRL,
        // Alt
        Key::KEY_LEFTALT,
        Key::KEY_RIGHTALT,
        // Windows
        Key::KEY_LEFTMETA,
        Key::KEY_RIGHTMETA,
    ];
}

//---

fn is_pressed(value: i32) -> bool {
    value == PRESS || value == REPEAT
}

// InputEvent#value
static RELEASE: i32 = 0;
static PRESS: i32 = 1;
static REPEAT: i32 = 2;

//---

#[derive(Debug)]
struct MultiPurposeKeyState {
    held: Key,
    alone: Key,
    // Some if the first press is still delayed, None if already considered held.
    alone_timeout_at: Option<Instant>,
}

impl MultiPurposeKeyState {
    fn repeat(&mut self) -> Vec<(Key, i32)> {
        if let Some(alone_timeout_at) = &self.alone_timeout_at {
            if Instant::now() < *alone_timeout_at {
                vec![] // still delay the press
            } else {
                self.alone_timeout_at = None; // timeout
                vec![(self.held, PRESS)]
            }
        } else {
            vec![(self.held, REPEAT)]
        }
    }

    fn release(&self) -> Vec<(Key, i32)> {
        if let Some(alone_timeout_at) = &self.alone_timeout_at {
            if Instant::now() < *alone_timeout_at {
                // dispatch the delayed press and this release
                vec![(self.alone, PRESS), (self.alone, RELEASE)]
            } else {
                // too late. dispatch the held key
                vec![(self.held, PRESS), (self.held, RELEASE)]
            }
        } else {
            vec![(self.held, RELEASE)]
        }
    }

    fn force_held(&mut self) -> Vec<(Key, i32)> {
        if self.alone_timeout_at.is_some() {
            self.alone_timeout_at = None;
            vec![(self.held, PRESS)]
        } else {
            vec![]
        }
    }
}

//! baseview (`keyboard_types`) -> iced keyboard-event translation.
//!
//! baseview delivers `Event::Keyboard` carrying `keyboard_types` data
//! whenever the window has OS focus; the translated event is pushed onto
//! the `UserInterface`'s event queue, so iced widgets (a focused
//! `text_input`, or a custom `Widget` matching `Event::Keyboard`) receive
//! it. Both `keyboard_types` and iced derive their key tables from the W3C
//! UI Events spec, so the physical `Code` and logical `Named` mappings are
//! name-for-name (a macro over the shared variants); keys iced does not
//! model fall back to `Unidentified`.

use crate::iced::keyboard::key::{Code, Named, NativeCode, Physical};
use crate::iced::keyboard::{Key, Location, Modifiers};
use keyboard_types::{KeyState, KeyboardEvent};

/// Translate a baseview keyboard event into an iced keyboard event.
pub(crate) fn to_iced_event(kb: &KeyboardEvent) -> crate::iced::keyboard::Event {
    let modifiers = convert_modifiers(kb.modifiers);
    let key = convert_key(&kb.key);
    let physical_key = convert_code(kb.code);
    let location = convert_location(kb.location);

    match kb.state {
        // baseview resolves one logical key (layout + modifiers already
        // applied), so `key` and `modified_key` carry the same value;
        // `physical_key` is the layout-independent code for whole-keyboard
        // and shortcut use.
        KeyState::Down => crate::iced::keyboard::Event::KeyPressed {
            modified_key: key.clone(),
            key,
            physical_key,
            location,
            // Committed text for insertion, suppressed under Ctrl/Cmd so a
            // shortcut like Ctrl+S does not also type "s" into a focused
            // field (iced shortcut handling reads `key` / `physical_key`).
            text: match &kb.key {
                keyboard_types::Key::Character(s)
                    if !modifiers.contains(Modifiers::CTRL)
                        && !modifiers.contains(Modifiers::LOGO) =>
                {
                    Some(s.as_str().into())
                }
                _ => None,
            },
            modifiers,
            repeat: kb.repeat,
        },
        KeyState::Up => crate::iced::keyboard::Event::KeyReleased {
            modified_key: key.clone(),
            key,
            physical_key,
            location,
            modifiers,
        },
    }
}

fn convert_modifiers(m: keyboard_types::Modifiers) -> Modifiers {
    let mut out = Modifiers::empty();
    if m.contains(keyboard_types::Modifiers::SHIFT) {
        out |= Modifiers::SHIFT;
    }
    if m.contains(keyboard_types::Modifiers::CONTROL) {
        out |= Modifiers::CTRL;
    }
    if m.contains(keyboard_types::Modifiers::ALT) {
        out |= Modifiers::ALT;
    }
    if m.contains(keyboard_types::Modifiers::META) {
        out |= Modifiers::LOGO;
    }
    out
}

fn convert_location(loc: keyboard_types::Location) -> Location {
    match loc {
        keyboard_types::Location::Standard => Location::Standard,
        keyboard_types::Location::Left => Location::Left,
        keyboard_types::Location::Right => Location::Right,
        keyboard_types::Location::Numpad => Location::Numpad,
    }
}

macro_rules! map_code {
    ($code:expr; $($v:ident),+ $(,)?) => {
        match $code {
            $( keyboard_types::Code::$v => Physical::Code(Code::$v), )+
            _ => Physical::Unidentified(NativeCode::Unidentified),
        }
    };
}

fn convert_code(code: keyboard_types::Code) -> Physical {
    map_code!(code;
        Abort, Again, AltLeft, AltRight, ArrowDown, ArrowLeft, ArrowRight, ArrowUp,
        AudioVolumeDown, AudioVolumeMute, AudioVolumeUp, Backquote, Backslash, Backspace,
        BracketLeft, BracketRight, BrowserBack, BrowserFavorites, BrowserForward,
        BrowserHome, BrowserRefresh, BrowserSearch, BrowserStop, CapsLock, Comma,
        ContextMenu, ControlLeft, ControlRight, Convert, Copy, Cut, Delete, Digit0, Digit1,
        Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9, Eject, End, Enter,
        Equal, Escape, F1, F10, F11, F12, F13, F14, F15, F16, F17, F18, F19, F2, F20, F21,
        F22, F23, F24, F25, F26, F27, F28, F29, F3, F30, F31, F32, F33, F34, F35, F4, F5,
        F6, F7, F8, F9, Find, Fn, FnLock, Help, Hiragana, Home, Hyper, Insert,
        IntlBackslash, IntlRo, IntlYen, KanaMode, Katakana, KeyA, KeyB, KeyC, KeyD, KeyE,
        KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN, KeyO, KeyP, KeyQ, KeyR, KeyS,
        KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ, Lang1, Lang2, Lang3, Lang4, Lang5,
        LaunchApp1, LaunchApp2, LaunchMail, MediaPlayPause, MediaSelect, MediaStop,
        MediaTrackNext, MediaTrackPrevious, Minus, NonConvert, NumLock, Numpad0, Numpad1,
        Numpad2, Numpad3, Numpad4, Numpad5, Numpad6, Numpad7, Numpad8, Numpad9, NumpadAdd,
        NumpadBackspace, NumpadClear, NumpadClearEntry, NumpadComma, NumpadDecimal,
        NumpadDivide, NumpadEnter, NumpadEqual, NumpadHash, NumpadMemoryAdd,
        NumpadMemoryClear, NumpadMemoryRecall, NumpadMemoryStore, NumpadMemorySubtract,
        NumpadMultiply, NumpadParenLeft, NumpadParenRight, NumpadStar, NumpadSubtract,
        Open, PageDown, PageUp, Paste, Pause, Period, Power, PrintScreen, Props, Quote,
        Resume, ScrollLock, Select, Semicolon, ShiftLeft, ShiftRight, Slash, Sleep, Space,
        Suspend, Tab, Turbo, Undo, WakeUp,
    )
}

macro_rules! map_named {
    ($key:expr; $($v:ident),+ $(,)?) => {
        match $key {
            keyboard_types::Key::Character(s) => Key::Character(s.as_str().into()),
            $( keyboard_types::Key::$v => Key::Named(Named::$v), )+
            _ => Key::Unidentified,
        }
    };
}

fn convert_key(key: &keyboard_types::Key) -> Key {
    map_named!(key;
        Accept, Again, AllCandidates, Alphanumeric, Alt, AltGraph, AppSwitch, ArrowDown,
        ArrowLeft, ArrowRight, ArrowUp, Attn, AudioBalanceLeft, AudioBalanceRight,
        AudioBassBoostDown, AudioBassBoostToggle, AudioBassBoostUp, AudioFaderFront,
        AudioFaderRear, AudioSurroundModeNext, AudioTrebleDown, AudioTrebleUp,
        AudioVolumeDown, AudioVolumeMute, AudioVolumeUp, AVRInput, AVRPower, Backspace,
        BrightnessDown, BrightnessUp, BrowserBack, BrowserFavorites, BrowserForward,
        BrowserHome, BrowserRefresh, BrowserSearch, BrowserStop, Call, Camera, CameraFocus,
        Cancel, CapsLock, ChannelDown, ChannelUp, Clear, Close, ClosedCaptionToggle,
        CodeInput, ColorF0Red, ColorF1Green, ColorF2Yellow, ColorF3Blue, ColorF4Grey,
        ColorF5Brown, Compose, ContextMenu, Control, Convert, Copy, CrSel, Cut, Delete,
        Dimmer, DisplaySwap, DVR, Eisu, Eject, End, EndCall, Enter, EraseEof, Escape,
        Execute, Exit, ExSel, F1, F10, F11, F12, F13, F14, F15, F16, F17, F18, F19, F2,
        F20, F21, F22, F23, F24, F25, F26, F27, F28, F29, F3, F30, F31, F32, F33, F34, F35,
        F4, F5, F6, F7, F8, F9, FavoriteClear0, FavoriteClear1, FavoriteClear2,
        FavoriteClear3, FavoriteRecall0, FavoriteRecall1, FavoriteRecall2, FavoriteRecall3,
        FavoriteStore0, FavoriteStore1, FavoriteStore2, FavoriteStore3, FinalMode, Find,
        Fn, FnLock, GoBack, GoHome, GroupFirst, GroupLast, GroupNext, GroupPrevious, Guide,
        GuideNextDay, GuidePreviousDay, HangulMode, HanjaMode, Hankaku, HeadsetHook, Help,
        Hibernate, Hiragana, HiraganaKatakana, Home, Hyper, Info, Insert, InstantReplay,
        JunjaMode, KanaMode, KanjiMode, Katakana, Key11, Key12, LastNumberRedial,
        LaunchApplication1, LaunchApplication2, LaunchCalendar, LaunchContacts, LaunchMail,
        LaunchMediaPlayer, LaunchMusicPlayer, LaunchPhone, LaunchScreenSaver,
        LaunchSpreadsheet, LaunchWebBrowser, LaunchWebCam, LaunchWordProcessor, Link,
        ListProgram, LiveContent, Lock, LogOff, MailForward, MailReply, MailSend,
        MannerMode, MediaApps, MediaAudioTrack, MediaClose, MediaFastForward, MediaLast,
        MediaPause, MediaPlay, MediaPlayPause, MediaRecord, MediaRewind, MediaSkipBackward,
        MediaSkipForward, MediaStepBackward, MediaStepForward, MediaStop, MediaTopMenu,
        MediaTrackNext, MediaTrackPrevious, Meta, MicrophoneToggle, MicrophoneVolumeDown,
        MicrophoneVolumeMute, MicrophoneVolumeUp, ModeChange, NavigateIn, NavigateNext,
        NavigateOut, NavigatePrevious, New, NextCandidate, NextFavoriteChannel,
        NextUserProfile, NonConvert, Notification, NumLock, OnDemand, Open, PageDown,
        PageUp, Pairing, Paste, Pause, PinPDown, PinPMove, PinPToggle, PinPUp, Play,
        PlaySpeedDown, PlaySpeedReset, PlaySpeedUp, Power, PowerOff, PreviousCandidate,
        Print, PrintScreen, Process, Props, RandomToggle, RcLowBattery, RecordSpeedNext,
        Redo, RfBypass, Romaji, Save, ScanChannelsToggle, ScreenModeNext, ScrollLock,
        Select, Settings, Shift, SingleCandidate, Soft1, Soft2, Soft3, Soft4,
        SpeechCorrectionList, SpeechInputToggle, SpellCheck, SplitScreenToggle, Standby,
        STBInput, STBPower, Subtitle, Super, Symbol, SymbolLock, Tab, Teletext, TV,
        TV3DMode, TVAntennaCable, TVAudioDescription, TVAudioDescriptionMixDown,
        TVAudioDescriptionMixUp, TVContentsMenu, TVDataService, TVInput, TVInputComponent1,
        TVInputComponent2, TVInputComposite1, TVInputComposite2, TVInputHDMI1,
        TVInputHDMI2, TVInputHDMI3, TVInputHDMI4, TVInputVGA1, TVMediaContext, TVNetwork,
        TVNumberEntry, TVPower, TVRadioService, TVSatellite, TVSatelliteBS, TVSatelliteCS,
        TVSatelliteToggle, TVTerrestrialAnalog, TVTerrestrialDigital, TVTimer, Undo,
        VideoModeNext, VoiceDial, WakeUp, Wink, Zenkaku, ZenkakuHankaku, ZoomIn, ZoomOut,
        ZoomToggle,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(state: KeyState, key: keyboard_types::Key, code: keyboard_types::Code) -> KeyboardEvent {
        KeyboardEvent {
            state,
            key,
            code,
            location: keyboard_types::Location::Standard,
            modifiers: keyboard_types::Modifiers::empty(),
            repeat: false,
            is_composing: false,
        }
    }

    #[test]
    fn character_down_carries_text_and_physical_code() {
        let e = ev(
            KeyState::Down,
            keyboard_types::Key::Character("a".into()),
            keyboard_types::Code::KeyA,
        );
        let crate::iced::keyboard::Event::KeyPressed {
            key,
            physical_key,
            text,
            ..
        } = to_iced_event(&e)
        else {
            panic!("expected KeyPressed");
        };
        assert_eq!(key, Key::Character("a".into()));
        assert_eq!(physical_key, Physical::Code(Code::KeyA));
        assert_eq!(text.as_deref(), Some("a"));
    }

    #[test]
    fn named_key_maps_by_name() {
        let e = ev(
            KeyState::Down,
            keyboard_types::Key::Enter,
            keyboard_types::Code::Enter,
        );
        let crate::iced::keyboard::Event::KeyPressed { key, .. } = to_iced_event(&e) else {
            panic!("expected KeyPressed");
        };
        assert_eq!(key, Key::Named(Named::Enter));
    }

    #[test]
    fn ctrl_suppresses_text_but_keeps_modifier() {
        let mut e = ev(
            KeyState::Down,
            keyboard_types::Key::Character("s".into()),
            keyboard_types::Code::KeyS,
        );
        e.modifiers = keyboard_types::Modifiers::CONTROL;
        let crate::iced::keyboard::Event::KeyPressed {
            text, modifiers, ..
        } = to_iced_event(&e)
        else {
            panic!("expected KeyPressed");
        };
        assert_eq!(text, None);
        assert!(modifiers.contains(Modifiers::CTRL));
    }

    #[test]
    fn key_up_is_a_release() {
        let e = ev(
            KeyState::Up,
            keyboard_types::Key::Character("a".into()),
            keyboard_types::Code::KeyA,
        );
        assert!(matches!(
            to_iced_event(&e),
            crate::iced::keyboard::Event::KeyReleased { .. }
        ));
    }

    // End-to-end check of the subscription pump's mechanism (the same
    // `Runtime` track / broadcast / drain the editor frame loop runs): a
    // `keyboard::listen` subscription must yield a message when a key event
    // is broadcast. Validates the pump without the GPU frame loop.
    #[test]
    fn subscription_pump_delivers_keyboard_events() {
        use iced_runtime::futures::{Runtime, subscription};
        use std::time::{Duration, Instant};

        let executor = crate::iced::futures::executor::ThreadPool::builder()
            .pool_size(1)
            .create()
            .expect("executor");
        let (tx, mut rx) = crate::iced::futures::channel::mpsc::unbounded::<u32>();
        let mut runtime = Runtime::new(executor, tx);

        // Any keyboard event -> message 7.
        let sub = crate::iced::keyboard::listen().map(|_event| 7u32);
        runtime.track(subscription::into_recipes(sub));

        let key = to_iced_event(&ev(
            KeyState::Down,
            keyboard_types::Key::Character("a".into()),
            keyboard_types::Code::KeyA,
        ));
        runtime.broadcast(subscription::Event::Interaction {
            window: crate::iced::window::Id::unique(),
            event: crate::iced::Event::Keyboard(key),
            status: crate::iced::event::Status::Ignored,
        });

        // The recipe stream runs on the worker thread; poll briefly.
        let deadline = Instant::now() + Duration::from_secs(2);
        let got = loop {
            if let Ok(m) = rx.try_recv() {
                break Some(m);
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(got, Some(7));
    }
}

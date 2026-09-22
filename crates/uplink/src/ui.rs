//! The Slint UI: tokens from docs/plan.md, a call screen that owns the frame, and the
//! people/identity/settings screens behind it.
//!
//! Layout is written in terms of start/end (mirrored by `Tokens.rtl`), never left/right, because
//! Slint does not mirror layouts itself and Arabic is a first-class language here.

slint::slint! {
    import { Button, LineEdit, ScrollView } from "std-widgets.slint";

    export enum Screen { call, people, identity, settings }
    export enum CallState { idle, dialing, ringing, incoming, connected }

    export struct ContactItem {
        name: string,
        id: string,
        fingerprint: string,
        initial: string,
        tint: int,
    }

    export global Tokens {
        out property <color> ink: #070A0F;
        out property <color> surface: #101720;
        out property <color> raised: #18212C;
        out property <color> line: #263140;
        out property <color> text: #E9F0F7;
        out property <color> muted: #93A3B5;
        out property <color> beacon: #58B6FF;
        out property <color> answer: #32C97A;
        out property <color> end: #FF5252;
        out property <length> gap: 8px;
        out property <length> radius: 14px;
        in-out property <bool> rtl: false;
        // Tints for generated avatars, by contact.
        out property <[color]> avatar-tints: [#D8E7F5, #F6DDE6, #DCEEDF, #F3E3CF, #E0DDF5];
    }

    component Avatar inherits Rectangle {
        in property <string> initial;
        in property <int> tint;
        in property <length> size: 36px;
        width: size;
        height: size;
        border-radius: size / 2;
        background: Tokens.avatar-tints[mod(tint, 5)];
        Text {
            text: initial;
            color: #123A5C;
            font-size: size / 2.4;
            font-weight: 600;
        }
    }

    component Key inherits Rectangle {
        in property <bool> on: false;
        in property <bool> danger: false;
        in property <bool> positive: false;
        in property <string> glyph;
        in property <string> label;
        callback clicked();
        width: danger || positive ? 62px : 48px;
        height: 48px;
        border-radius: 24px;
        background: danger ? Tokens.end
            : positive ? Tokens.answer
            : on ? Tokens.text
            : #E9F0F71A;
        border-width: danger || positive || on ? 0px : 1px;
        border-color: #E9F0F72B;
        accessible-role: button;
        accessible-label: label;
        accessible-action-default => { root.clicked(); }
        Text {
            text: glyph;
            font-size: 18px;
            color: danger ? #FFFFFF : positive ? #03301B : on ? Tokens.ink : Tokens.text;
        }
        touch := TouchArea {
            clicked => { root.clicked(); }
        }
        states [
            pressed when touch.pressed : {
                background: danger ? Tokens.end.darker(20%) : positive ? Tokens.answer.darker(20%) : #E9F0F733;
            }
        ]
        animate background { duration: 160ms; easing: ease-out; }
    }

    component Chip inherits Rectangle {
        in property <string> text;
        in property <color> tone: Tokens.beacon;
        height: 20px;
        border-radius: 10px;
        background: tone.with-alpha(0.16);
        border-width: 1px;
        border-color: tone.with-alpha(0.35);
        HorizontalLayout {
            padding-left: 8px;
            padding-right: 8px;
            Text {
                text: root.text;
                color: tone;
                font-size: 10px;
                vertical-alignment: center;
            }
        }
    }

    component TabBar inherits Rectangle {
        in-out property <Screen> screen;
        height: 56px;
        background: Tokens.surface;
        border-width: 1px;
        border-color: Tokens.line;
        HorizontalLayout {
            padding: 4px;
            spacing: 4px;
            for tab in [
                { label: "Call", screen: Screen.call },
                { label: "People", screen: Screen.people },
                { label: "Key", screen: Screen.identity },
                { label: "Settings", screen: Screen.settings },
            ] : Rectangle {
                border-radius: 12px;
                background: root.screen == tab.screen ? Tokens.raised : transparent;
                Text {
                    text: tab.label;
                    font-size: 12px;
                    color: root.screen == tab.screen ? Tokens.beacon : Tokens.muted;
                }
                TouchArea {
                    clicked => { root.screen = tab.screen; }
                }
            }
        }
    }

    export component App inherits Window {
        in property <image> frame;
        in property <image> remote-frame;
        in property <image> qr;
        in property <string> my-id;
        in property <string> my-fingerprint;
        in property <string> stats;
        in property <string> log;
        in property <string> call-status;
        in property <string> peer-name;
        in property <string> call-timer;
        in property <string> key-exchange;
        in property <[ContactItem]> contacts;
        in property <bool> camera-running;
        // First letter of peer-name; Slint has no string indexing, so Rust supplies it.
        in property <string> peer-initial: "?";

        in-out property <Screen> screen: Screen.call;
        in-out property <bool> log-open: false;
        in-out property <bool> scanning: false;
        in-out property <bool> swapped: false;
        in-out property <bool> mic-on: true;
        in-out property <bool> speaker-on: true;
        in-out property <string> peer-key;
        in-out property <string> new-name;
        in property <CallState> call-state: CallState.idle;

        callback call(string);
        callback accept();
        callback reject();
        callback hangup();
        callback flip-camera();
        callback toggle-mic();
        callback toggle-speaker();
        callback add-contact(string, string);
        callback rename-contact(string, string);
        callback remove-contact(string);
        callback pick-key();
        callback scan(bool);
        callback start-camera();

        title: "uplink";
        background: Tokens.ink;
        default-font-size: 14px;

        // ---------- call ----------
        if root.screen == Screen.call || root.call-state != CallState.idle : Rectangle {
            background: Tokens.ink;

            // Remote video fills the frame once connected; before that it is our own camera.
            main := Image {
                width: parent.width;
                height: parent.height;
                source: root.call-state == CallState.connected && !root.swapped ? root.remote-frame : root.frame;
                image-fit: cover;
            }

            // Self view: full frame until the call connects, then a corner card.
            self-view := Rectangle {
                property <bool> inset: root.call-state == CallState.connected && !root.swapped;
                x: inset ? (Tokens.rtl ? 16px : parent.width - self.width - 16px) : 0px;
                y: inset ? 76px : 0px;
                width: inset ? 96px : parent.width;
                height: inset ? 142px : parent.height;
                border-radius: inset ? 14px : 0px;
                border-width: inset ? 1px : 0px;
                border-color: #FFFFFF2E;
                clip: true;
                visible: root.call-state == CallState.connected;
                Image {
                    width: parent.width;
                    height: parent.height;
                    source: root.swapped ? root.remote-frame : root.frame;
                    image-fit: cover;
                }
                TouchArea {
                    clicked => { root.swapped = !root.swapped; }
                }
                animate x, y, width, height, border-radius { duration: 620ms; easing: ease-out-quint; }
            }

            // Top: who, how long, how secure.
            VerticalLayout {
                padding-top: root.safe-area-insets.top + 12px;
                padding-left: 16px;
                padding-right: 16px;
                alignment: start;
                spacing: 6px;
                HorizontalLayout {
                    alignment: space-between;
                    spacing: 8px;
                    VerticalLayout {
                        spacing: 2px;
                        Text {
                            text: root.call-state == CallState.idle ? "uplink" : root.peer-name;
                            color: Tokens.text;
                            font-size: 20px;
                            font-weight: 600;
                        }
                        Text {
                            text: root.call-state == CallState.connected ? root.call-timer : root.call-status;
                            color: Tokens.muted;
                            font-size: 12px;
                            font-family: root.call-state == CallState.connected ? "monospace" : "";
                        }
                    }
                    if root.call-state == CallState.connected && root.key-exchange != "" : Chip {
                        text: root.key-exchange;
                    }
                    if root.call-state == CallState.idle : Button {
                        text: "Log";
                        clicked => { root.log-open = true; }
                    }
                }
                if root.call-state == CallState.idle : Text {
                    text: root.stats;
                    color: Tokens.muted;
                    font-size: 10px;
                    wrap: word-wrap;
                }
            }

            // Ringing / incoming: name and avatar over the (dimmed) camera.
            if root.call-state == CallState.incoming || root.call-state == CallState.dialing || root.call-state == CallState.ringing : Rectangle {
                background: #070A0FA6;
                VerticalLayout {
                    alignment: center;
                    spacing: 14px;
                    HorizontalLayout {
                        alignment: center;
                        Avatar {
                            initial: root.peer-initial;
                            tint: 1;
                            size: 92px;
                        }
                    }
                    Text {
                        text: root.peer-name;
                        color: Tokens.text;
                        font-size: 24px;
                        font-weight: 600;
                        horizontal-alignment: center;
                    }
                    Text {
                        text: root.call-state == CallState.incoming ? "Incoming video call" : "Calling…";
                        color: Tokens.muted;
                        font-size: 13px;
                        horizontal-alignment: center;
                    }
                }
            }

            // Controls.
            VerticalLayout {
                alignment: end;
                padding-bottom: max(root.safe-area-insets.bottom, 12px) + 12px;
                HorizontalLayout {
                    alignment: center;
                    spacing: 10px;
                    if root.call-state == CallState.idle : Key {
                        glyph: root.camera-running ? "◉" : "○";
                        label: "Camera";
                        on: root.camera-running;
                        clicked => { root.start-camera(); }
                    }
                    if root.call-state == CallState.idle : Key {
                        glyph: "⇄";
                        label: "Flip camera";
                        clicked => { root.flip-camera(); }
                    }
                    if root.call-state != CallState.idle && root.call-state != CallState.incoming : Key {
                        glyph: root.mic-on ? "🎙" : "🔇";
                        label: "Microphone";
                        on: !root.mic-on;
                        clicked => { root.toggle-mic(); }
                    }
                    if root.call-state == CallState.connected : Key {
                        glyph: "🔊";
                        label: "Speaker";
                        on: root.speaker-on;
                        clicked => { root.toggle-speaker(); }
                    }
                    if root.call-state == CallState.connected : Key {
                        glyph: "⇄";
                        label: "Flip camera";
                        clicked => { root.flip-camera(); }
                    }
                    if root.call-state == CallState.incoming : Key {
                        glyph: "✕";
                        label: "Decline";
                        danger: true;
                        clicked => { root.reject(); }
                    }
                    if root.call-state == CallState.incoming : Key {
                        glyph: "✆";
                        label: "Answer";
                        positive: true;
                        clicked => { root.accept(); }
                    }
                    if root.call-state != CallState.idle && root.call-state != CallState.incoming : Key {
                        glyph: "✆";
                        label: "End call";
                        danger: true;
                        clicked => { root.hangup(); }
                    }
                }
            }
        }

        // ---------- people ----------
        if root.screen == Screen.people && root.call-state == CallState.idle : Rectangle {
            background: Tokens.ink;
            VerticalLayout {
                padding-top: root.safe-area-insets.top + 12px;
                padding-bottom: 68px;
                padding-left: 16px;
                padding-right: 16px;
                spacing: 12px;
                Text {
                    text: "People";
                    color: Tokens.text;
                    font-size: 22px;
                    font-weight: 600;
                }
                ScrollView {
                    vertical-stretch: 1;
                    VerticalLayout {
                        spacing: 6px;
                        alignment: start;
                        for contact[index] in root.contacts : Rectangle {
                            height: 60px;
                            border-radius: Tokens.radius;
                            background: Tokens.surface;
                            HorizontalLayout {
                                padding: 10px;
                                spacing: 10px;
                                Avatar {
                                    initial: contact.initial;
                                    tint: index;
                                }
                                VerticalLayout {
                                    alignment: center;
                                    spacing: 2px;
                                    horizontal-stretch: 1;
                                    Text {
                                        text: contact.name;
                                        color: Tokens.text;
                                        font-size: 15px;
                                        font-weight: 600;
                                        overflow: elide;
                                    }
                                    Text {
                                        text: contact.fingerprint;
                                        color: Tokens.muted;
                                        font-size: 10px;
                                        font-family: "monospace";
                                        overflow: elide;
                                    }
                                }
                                Key {
                                    glyph: "✆";
                                    label: "Call " + contact.name;
                                    positive: true;
                                    clicked => { root.call(contact.id); }
                                }
                                Key {
                                    glyph: "✕";
                                    label: "Remove " + contact.name;
                                    clicked => { root.remove-contact(contact.id); }
                                }
                            }
                        }
                        if root.contacts.length == 0 : Text {
                            text: "No one yet. Add someone from the Key screen — scan their code, or paste it below.";
                            color: Tokens.muted;
                            font-size: 13px;
                            wrap: word-wrap;
                        }
                    }
                }
                VerticalLayout {
                    spacing: 6px;
                    LineEdit {
                        placeholder-text: "Name";
                        text <=> root.new-name;
                    }
                    LineEdit {
                        placeholder-text: "Their key";
                        text <=> root.peer-key;
                        font-size: 12px;
                    }
                    Button {
                        text: "Add contact";
                        primary: true;
                        enabled: root.new-name != "" && root.peer-key != "";
                        clicked => { root.add-contact(root.new-name, root.peer-key); }
                    }
                }
            }
        }

        // ---------- identity ----------
        if root.screen == Screen.identity && root.call-state == CallState.idle : Rectangle {
            background: Tokens.ink;
            VerticalLayout {
                padding-top: root.safe-area-insets.top + 12px;
                padding-bottom: 68px;
                padding-left: 16px;
                padding-right: 16px;
                spacing: 12px;
                Text {
                    text: root.scanning ? "Point at their code" : "Your key";
                    color: Tokens.text;
                    font-size: 22px;
                    font-weight: 600;
                }
                // Square, so neither the code nor the camera is ever cropped oddly.
                Rectangle {
                    vertical-stretch: 1;
                    clip: true;
                    qr-card := Rectangle {
                        // Square from the width alone: taking the parent's height back would
                        // make the layout depend on itself.
                        width: min(parent.width, 320px);
                        height: self.width;
                        border-radius: Tokens.radius;
                        background: root.scanning ? Tokens.surface : #FFFFFF;
                        clip: true;
                        Image {
                            width: parent.width;
                            height: parent.height;
                            source: root.scanning ? root.frame : root.qr;
                            image-fit: root.scanning ? ImageFit.cover : ImageFit.contain;
                        }
                    }
                }
                Text {
                    text: root.scanning ? "" : root.my-fingerprint;
                    color: Tokens.muted;
                    font-size: 13px;
                    font-family: "monospace";
                    horizontal-alignment: center;
                    wrap: word-wrap;
                }
                Text {
                    text: root.scanning ? "Hold their code steady in the frame" : "Send them this code, or read theirs.";
                    color: Tokens.muted;
                    font-size: 12px;
                    horizontal-alignment: center;
                    wrap: word-wrap;
                }
                HorizontalLayout {
                    spacing: 8px;
                    Button {
                        text: root.scanning ? "Stop" : "Scan a code";
                        primary: !root.scanning;
                        clicked => { root.scan(!root.scanning); }
                    }
                    if !root.scanning : Button {
                        text: "From an image";
                        clicked => { root.pick-key(); }
                    }
                }
            }
        }

        // ---------- settings ----------
        if root.screen == Screen.settings && root.call-state == CallState.idle : Rectangle {
            background: Tokens.ink;
            VerticalLayout {
                padding-top: root.safe-area-insets.top + 12px;
                padding-bottom: 68px;
                padding-left: 16px;
                padding-right: 16px;
                spacing: 12px;
                Text {
                    text: "Settings";
                    color: Tokens.text;
                    font-size: 22px;
                    font-weight: 600;
                }
                ScrollView {
                    vertical-stretch: 1;
                    VerticalLayout {
                        spacing: 8px;
                        alignment: start;
                        Rectangle {
                            height: 58px;
                            border-radius: Tokens.radius;
                            background: Tokens.surface;
                            HorizontalLayout {
                                padding: 12px;
                                spacing: 10px;
                                VerticalLayout {
                                    alignment: center;
                                    horizontal-stretch: 1;
                                    Text { text: "Diagnostics"; color: Tokens.text; font-size: 14px; }
                                    Text { text: "This run's log"; color: Tokens.muted; font-size: 11px; }
                                }
                                Button {
                                    text: "Open";
                                    clicked => { root.log-open = true; }
                                }
                            }
                        }
                        Rectangle {
                            height: 58px;
                            border-radius: Tokens.radius;
                            background: Tokens.surface;
                            HorizontalLayout {
                                padding: 12px;
                                spacing: 10px;
                                VerticalLayout {
                                    alignment: center;
                                    horizontal-stretch: 1;
                                    Text { text: "Layout direction"; color: Tokens.text; font-size: 14px; }
                                    Text { text: "Mirrors the screens (Arabic)"; color: Tokens.muted; font-size: 11px; }
                                }
                                Button {
                                    text: Tokens.rtl ? "Right to left" : "Left to right";
                                    clicked => { Tokens.rtl = !Tokens.rtl; }
                                }
                            }
                        }
                        Text {
                            text: root.stats;
                            color: Tokens.muted;
                            font-size: 10px;
                            font-family: "monospace";
                            wrap: word-wrap;
                        }
                    }
                }
            }
        }

        // ---------- tabs ----------
        if root.call-state == CallState.idle : VerticalLayout {
            alignment: end;
            padding-bottom: max(root.safe-area-insets.bottom, 8px);
            padding-left: 8px;
            padding-right: 8px;
            TabBar {
                screen <=> root.screen;
            }
        }

        // ---------- log overlay ----------
        if root.log-open : Rectangle {
            background: #000000F0;
            VerticalLayout {
                padding-top: root.safe-area-insets.top + 12px;
                padding-bottom: max(root.safe-area-insets.bottom, 12px);
                padding-left: 16px;
                padding-right: 16px;
                spacing: 10px;
                HorizontalLayout {
                    alignment: space-between;
                    Text { text: "Log"; color: Tokens.text; font-size: 18px; font-weight: 600; }
                    Button { text: "Close"; clicked => { root.log-open = false; } }
                }
                ScrollView {
                    vertical-stretch: 1;
                    Text {
                        text: root.log;
                        color: Tokens.muted;
                        font-size: 10px;
                        font-family: "monospace";
                        wrap: word-wrap;
                    }
                }
            }
        }
    }
}

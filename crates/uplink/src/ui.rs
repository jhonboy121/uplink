//! The Slint UI: tokens from docs/plan.md, a call screen that owns the frame, and the
//! people/identity/settings screens behind it.
//!
//! Structure: one root `VerticalLayout` holds the page area and the tab bar, so the bar takes
//! space instead of covering content; the in-call screen and the log are declared after it, as
//! overlays (later siblings draw on top). Layout is written start/end (mirrored by `Theme.rtl`),
//! never left/right, because Slint does not mirror layouts itself.

slint::slint! {
    // Assets are addressed from here, not by a relative path repeated at every use. The path is
    // resolved against this file, so the app and the preview tool both find them.
    #[include_path = "../../../assets"]

    import { Button, LineEdit, ScrollView, VerticalBox, HorizontalBox, Palette } from "std-widgets.slint";

    // The design's three faces, vendored in assets/fonts and registered from the markup itself,
    // so no build script is involved. Outfit names things, Public Sans carries prose, and
    // IBM Plex Mono is for anything read character by character: keys, fingerprints, the timer.
    import "fonts/Outfit-Regular.ttf";
    import "fonts/Outfit-SemiBold.ttf";
    import "fonts/PublicSans-Regular.ttf";
    import "fonts/PublicSans-SemiBold.ttf";
    import "fonts/PublicSans-Bold.ttf";
    import "fonts/IBMPlexMono-Regular.ttf";
    import "fonts/IBMPlexMono-Medium.ttf";

    // No "call" screen: there is nothing to show when no call is running, and a call takes over
    // the whole window when there is one.
    export enum Screen { people, identity, settings }
    export enum CallState { idle, dialing, ringing, incoming, connected }

    export struct ContactItem {
        name: string,
        id: string,
        fingerprint: string,
        initial: string,
        tint: int,
    }

    export enum Appearance { system, light, dark }

    export global Theme {
        // Lists and settings follow the system; a call is always dark, because video wants a dark
        // room, so chrome over video uses the fixed `on-video` tokens whatever the theme is.
        in-out property <Appearance> appearance: Appearance.system;
        // Only read while following the system, so forcing a mode leaves no dependency on the
        // palette and the two cannot drive each other.
        out property <bool> system-dark: Palette.color-scheme == ColorScheme.dark;
        out property <bool> dark: appearance == Appearance.system
            ? system-dark
            : appearance == Appearance.dark;

        out property <color> ground: dark ? #070A0F : #FFFFFF;
        out property <color> surface: dark ? #101720 : #F1F4F8;
        out property <color> raised: dark ? #18212C : #E4EAF1;
        out property <color> text: dark ? #E9F0F7 : #0E151E;
        out property <color> muted: dark ? #93A3B5 : #6B7C8F;
        out property <color> beacon: dark ? #58B6FF : #0A6FC2;
        // Rows are separated by a hairline, not by gaps between cards.
        out property <color> hairline: dark ? #2631408C : #E7ECF2;

        // Fixed: a call screen is dark in either theme.
        out property <color> video: #101720;
        out property <color> on-video: #E9F0F7;
        out property <color> on-video-muted: #93A3B5;
        out property <color> on-video-accent: #A9D9FF;
        out property <color> under-video: #070A0F;
        out property <color> answer: #32C97A;
        out property <color> end: #FF5252;
        // One spacing scale: 8, halved to 4 where tight.
        out property <length> gap: 8px;
        out property <length> gap-large: 16px;
        out property <length> radius: 14px;
        // List metrics, measured off the design in a browser and scaled to a 360dp screen.
        out property <length> edge: 20px;
        out property <length> row-height: 72px;
        out property <length> row-gap: 13px;
        out property <length> value-gap: 15px;
        out property <length> avatar: 44px;
        out property <length> bar-top: 12px;
        out property <length> bar-bottom: 15px;
        out property <length> pill-height: 28px;
        out property <length> detail-height: 74px;
        // Call chrome, measured off the design's call frames. `px` is Slint's logical pixel —
        // it is dp, scaled by the display factor, so these are device-independent already.
        out property <length> key-size: 49px;
        out property <length> key-wide: 64px;
        out property <length> key-icon: 22px;
        out property <length> key-gap: 13px;
        out property <length> call-edge: 19px;
        out property <length> call-top: 15px;
        out property <length> call-bottom: 23px;
        out property <length> chip-height: 31px;
        out property <length> chip-pad: 10px;
        out property <length> self-view-width: 90px;
        out property <length> self-view-height: 134px;
        out property <length> self-view-edge: 17px;
        out property <length> self-view-top: 67px;
        out property <length> peer-avatar: 110px;
        // The Add-someone stack.
        out property <length> wide-height: 44px;
        out property <length> wide-radius: 15px;
        out property <length> stack-gap: 12px;
        out property <length> stack-top: 17px;
        out property <length> qr-size: 171px;
        out property <length> qr-pad: 12px;
        // The design sets every line box to 1.6x its font; Slint's own is far tighter, so text
        // gets the taller box explicitly and centres in it.
        out property <float> line-box: 1.6;
        out property <string> display: "Outfit";
        out property <string> mono: "IBM Plex Mono";
        // In the design's order: the first contact is rose, the second blue. Each tint carries
        // its own ink, as the design does — the initial is never navy on a rose circle.
        out property <[color]> avatar-tints: [#F6DDE6, #D8E7F5, #DCEEDF, #F3E3CF, #E0DDF5];
        out property <[color]> avatar-inks: [#62243C, #123A5C, #1D4A2C, #5C4218, #332C5E];
        in-out property <bool> rtl: false;
    }

    component Avatar inherits Rectangle {
        in property <string> initial;
        in property <int> tint;
        in property <length> size: 40px;
        width: size;
        height: size;
        border-radius: size / 2;
        background: Theme.avatar-tints[mod(tint, 5)];
        Text {
            text: initial;
            color: Theme.avatar-inks[mod(tint, 5)];
            font-family: Theme.display;
            font-size: size / 2.6;
            font-weight: 600;
        }
    }

    // A round call control: custom rather than a Button because these are circles over video,
    // but it still reacts and carries an accessible label.
    component Key inherits Rectangle {
        in property <bool> on: false;
        in property <bool> danger: false;
        in property <bool> positive: false;
        in property <image> icon;
        in property <string> label;
        callback clicked();
        width: danger || positive ? Theme.key-wide : Theme.key-size;
        height: Theme.key-size;
        border-radius: self.height / 2;
        background: danger ? Theme.end
            : positive ? Theme.answer
            : on ? Theme.on-video
            : #E9F0F719;
        border-width: danger || positive || on ? 0px : 1px;
        border-color: #E9F0F728;
        accessible-role: button;
        accessible-label: label;
        accessible-action-default => { root.clicked(); }
        Image {
            source: root.icon;
            width: Theme.key-icon;
            height: Theme.key-icon;
            colorize: danger || positive ? #FFFFFF : on ? Theme.under-video : Theme.on-video;
        }
        touch := TouchArea {
            mouse-cursor: pointer;
            clicked => { root.clicked(); }
        }
        states [
            pressed when touch.pressed : { opacity: 0.6; }
            hovered when touch.has-hover : { opacity: 0.85; }
        ]
        animate opacity, background { duration: 160ms; easing: ease-out; }
    }

    component Card inherits Rectangle {
        border-radius: Theme.radius;
        background: Theme.surface;
        @children
    }

    component PageTitle inherits Text {
        color: Theme.text;
        font-family: Theme.display;
        font-size: 1.4375rem;
        font-weight: 600;
        height: self.font-size * Theme.line-box;
        vertical-alignment: center;
    }

    component Tabs inherits Rectangle {
        in-out property <Screen> screen;
        // The gesture bar sits under the tabs, so the row clears it from the inside.
        in property <length> bottom-inset;
        height: 64px + bottom-inset;
        background: Theme.surface;
        HorizontalLayout {
            padding: 6px;
            padding-bottom: 6px + root.bottom-inset;
            spacing: 4px;
            for tab in [
                { label: "People", screen: Screen.people },
                { label: "Key", screen: Screen.identity },
                { label: "Settings", screen: Screen.settings },
            ] : Rectangle {
                border-radius: 12px;
                background: root.screen == tab.screen ? Theme.raised : transparent;
                Text {
                    text: tab.label;
                    font-size: 0.85rem;
                    color: root.screen == tab.screen ? Theme.beacon : Theme.muted;
                }
                TouchArea {
                    mouse-cursor: pointer;
                    clicked => { root.screen = tab.screen; }
                }
                animate background { duration: 160ms; easing: ease-out; }
            }
        }
    }

    // The design's one button shape: an outlined pill, for both the header action and Call.
    component Pill inherits Rectangle {
        in property <string> text;
        callback clicked();
        height: Theme.pill-height;
        width: label.preferred-width + 28px;
        border-radius: self.height / 2;
        background: transparent;
        border-width: 1px;
        border-color: Theme.beacon.with-alpha(0.35);
        accessible-role: button;
        accessible-label: text;
        accessible-action-default => { root.clicked(); }
        label := Text {
            text: root.text;
            color: Theme.beacon;
            font-size: 0.8125rem;
        }
        touch := TouchArea {
            mouse-cursor: pointer;
            clicked => { root.clicked(); }
        }
        states [ pressed when touch.pressed : { opacity: 0.6; } ]
        animate opacity { duration: 160ms; easing: ease-out; }
    }

    // The design's full-width action. Its secondary colours were written for a dark ground, so
    // here they come from the accent rather than the literal pale blue, to stay readable in light.
    component Wide inherits Rectangle {
        in property <string> text;
        in property <bool> primary: false;
        in property <bool> enabled: true;
        callback clicked();
        height: Theme.wide-height;
        border-radius: Theme.wide-radius;
        background: root.primary ? Theme.beacon : Theme.beacon.with-alpha(0.12);
        border-width: root.primary ? 0px : 1px;
        border-color: Theme.beacon.with-alpha(0.30);
        opacity: root.enabled ? 1.0 : 0.45;
        accessible-role: button;
        accessible-label: text;
        accessible-action-default => { root.clicked(); }
        Text {
            text: root.text;
            color: root.primary ? (Theme.dark ? #04121F : #FFFFFF) : Theme.beacon;
            font-size: 1rem;
            font-weight: root.primary ? 600 : 400;
            height: self.font-size * Theme.line-box;
            vertical-alignment: center;
        }
        touch := TouchArea {
            mouse-cursor: root.enabled ? pointer : default;
            clicked => {
                if (root.enabled) {
                    root.clicked();
                }
            }
        }
        states [ pressed when touch.pressed && root.enabled : { opacity: 0.6; } ]
        animate opacity { duration: 160ms; easing: ease-out; }
    }

    // What a thing is on the start side, what it is set to on the end side. Ends the People
    // list with your own key, and makes up the Settings list.
    component DetailRow inherits Rectangle {
        in property <string> title;
        in property <string> detail;
        in property <string> value;
        in property <bool> tappable: true;
        callback clicked();
        height: Theme.detail-height;
        accessible-role: button;
        accessible-label: title;
        accessible-action-default => { root.clicked(); }
        // The design gives every one of these a hairline above it, the first included, and counts
        // it in the row's height.
        Rectangle {
            y: 0;
            height: 1px;
            background: Theme.hairline;
        }
        touch := TouchArea {
            mouse-cursor: root.tappable ? pointer : default;
            clicked => {
                if (root.tappable) {
                    root.clicked();
                }
            }
        }
        background: touch.pressed && root.tappable ? Theme.surface : transparent;
        animate background { duration: 160ms; easing: ease-out; }
        HorizontalLayout {
            y: 1px;
            height: parent.height - 1px;
            padding-left: Theme.edge;
            padding-right: Theme.edge;
            spacing: Theme.value-gap;
            VerticalLayout {
                alignment: center;
                horizontal-stretch: 1;
                Text {
                    text: root.title;
                    color: Theme.text;
                    font-size: 1rem;
                    font-weight: 700;
                    height: self.font-size * Theme.line-box;
                    vertical-alignment: center;
                    overflow: elide;
                }
                Text {
                    text: root.detail;
                    color: Theme.muted;
                    font-size: 0.8125rem;
                    height: self.font-size * Theme.line-box;
                    vertical-alignment: center;
                    overflow: elide;
                }
            }
            VerticalLayout {
                alignment: center;
                Text {
                    text: root.value;
                    color: root.tappable ? Theme.beacon : Theme.muted;
                    font-size: 0.908rem;
                    font-family: Theme.mono;
                    height: self.font-size * Theme.line-box;
                    vertical-alignment: center;
                }
            }
        }
    }

    component PeoplePage inherits Rectangle {
        in property <[ContactItem]> contacts;
        in property <length> top-inset;
        in property <length> bottom-inset;
        in property <string> status;
        in property <string> fingerprint;
        callback call(string);
        callback add-someone();
        callback show-key();
        background: Theme.ground;

        VerticalLayout {
            // App bar: the screen's name, and its one action.
            HorizontalLayout {
                vertical-stretch: 0;
                padding-top: root.top-inset + Theme.bar-top;
                padding-left: Theme.edge;
                padding-right: Theme.edge;
                padding-bottom: Theme.bar-bottom;
                spacing: Theme.gap;
                PageTitle {
                    text: "People";
                    horizontal-stretch: 1;
                    vertical-alignment: center;
                }
                VerticalLayout {
                    alignment: center;
                    Pill {
                        text: "+ Add";
                        clicked => { root.add-someone(); }
                    }
                }
            }
            // Only ever set when something went wrong; the design has no room for it otherwise.
            if root.status != "" : Text {
                text: root.status;
                color: Theme.beacon;
                font-size: 0.8125rem;
                wrap: word-wrap;
            }

            if root.contacts.length == 0 : Rectangle {
                vertical-stretch: 1;
                VerticalLayout {
                    alignment: center;
                    spacing: Theme.gap;
                    padding-left: Theme.edge;
                    padding-right: Theme.edge;
                    Text {
                        text: "No one here yet";
                        color: Theme.text;
                        font-size: 1.0625rem;
                        horizontal-alignment: center;
                    }
                    Text {
                        text: "Add someone from their code, and they appear here to call.";
                        color: Theme.muted;
                        font-size: 0.875rem;
                        horizontal-alignment: center;
                        wrap: word-wrap;
                    }
                }
            }
            if root.contacts.length > 0 : ScrollView {
                vertical-stretch: 1;
                VerticalLayout {
                    alignment: start;
                    // A flat list: rows are divided by a hairline, never boxed. The hairline is
                    // part of the stack, as the design's border-top is, so the pitch includes it.
                    for contact[index] in root.contacts : VerticalLayout {
                        if index > 0 : Rectangle {
                            height: 1px;
                            background: Theme.hairline;
                        }
                        Rectangle {
                            height: Theme.row-height;
                            HorizontalLayout {
                                padding-left: Theme.edge;
                                padding-right: Theme.edge;
                                spacing: Theme.row-gap;
                                VerticalLayout {
                                    alignment: center;
                                    Avatar {
                                        initial: contact.initial;
                                        tint: index;
                                        size: Theme.avatar;
                                    }
                                }
                                VerticalLayout {
                                    alignment: center;
                                    spacing: 1px;
                                    horizontal-stretch: 1;
                                    Text {
                                        text: contact.name;
                                        color: Theme.text;
                                        font-family: Theme.display;
                                        font-size: 1.0625rem;
                                        font-weight: 600;
                                        height: self.font-size * Theme.line-box;
                                        vertical-alignment: center;
                                        overflow: elide;
                                    }
                                    Text {
                                        text: contact.fingerprint;
                                        color: Theme.muted;
                                        // 12.34dp, not 12: the design's sizes are fractional, and
                                        // rounding one costs five over a run this long.
                                        font-size: 0.771rem;
                                        font-family: Theme.mono;
                                        height: self.font-size * Theme.line-box;
                                        vertical-alignment: center;
                                        overflow: elide;
                                    }
                                }
                                VerticalLayout {
                                    alignment: center;
                                    Pill {
                                        text: "Call";
                                        clicked => { root.call(contact.id); }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Your own key closes the screen, the way the design ends the list.
            DetailRow {
                title: "Your key";
                detail: "Tap to show the QR";
                value: root.fingerprint;
                clicked => { root.show-key(); }
            }
            // The gesture bar sits below the last row rather than over it.
            Rectangle { height: root.bottom-inset; }
        }
    }

    component KeyPage inherits Rectangle {
        in property <length> top-inset;
        in property <image> qr;
        in property <image> frame;
        in property <[string]> fingerprint-lines;
        in property <bool> scanning;
        in property <string> pending-key;
        in-out property <string> new-name;
        callback scan(bool);
        callback pick();
        callback share();
        callback close();
        callback add(string, string);
        background: Theme.ground;

        VerticalLayout {
            HorizontalLayout {
                vertical-stretch: 0;
                padding-top: root.top-inset + Theme.bar-top;
                padding-left: Theme.edge;
                padding-right: Theme.edge;
                padding-bottom: Theme.bar-bottom;
                spacing: Theme.gap;
                PageTitle {
                    text: root.scanning ? "Point at their code" : "Add someone";
                    horizontal-stretch: 1;
                }
                VerticalLayout {
                    alignment: center;
                    Pill {
                        text: root.scanning ? "Stop" : "Close";
                        clicked => {
                            if (root.scanning) {
                                root.scan(false);
                            } else {
                                root.close();
                            }
                        }
                    }
                }
            }

            // Code, fingerprint and actions are one stack, as the design has them; the slack
            // collects below rather than pushing them apart.
            VerticalLayout {
                vertical-stretch: 0;
                padding-left: Theme.edge;
                padding-right: Theme.edge;
                padding-top: Theme.stack-top;
                spacing: Theme.stack-gap;
                HorizontalLayout {
                    alignment: center;
                    Rectangle {
                        width: min(root.width - Theme.edge * 2, Theme.qr-size);
                        height: self.width;
                        border-radius: Theme.wide-radius;
                        background: root.scanning ? Theme.surface : #FFFFFF;
                        clip: true;
                        Image {
                            width: parent.width - (root.scanning ? 0px : Theme.qr-pad * 2);
                            height: self.width;
                            source: root.scanning ? root.frame : root.qr;
                            image-fit: root.scanning ? ImageFit.cover : ImageFit.contain;
                        }
                    }
                }
                // One Text per line: Slint has no line-height, so a wrapped string would set its
                // own leading and the design's 1.6 line box would be lost.
                if root.scanning : Text {
                    text: "Hold their code steady in the frame";
                    color: Theme.muted;
                    font-size: 0.9375rem;
                    height: self.font-size * Theme.line-box;
                    horizontal-alignment: center;
                    vertical-alignment: center;
                    wrap: word-wrap;
                }
                if !root.scanning : VerticalLayout {
                    for line in root.fingerprint-lines : Text {
                        text: line;
                        color: Theme.muted;
                        font-size: 0.9375rem;
                        font-family: Theme.mono;
                        letter-spacing: 0.6px;
                        height: self.font-size * Theme.line-box;
                        horizontal-alignment: center;
                        vertical-alignment: center;
                    }
                }

                // Once a key is in hand (scanned, read from an image, or pasted) this is all that
                // is left to do: give them a name. The design has no state for it, so it borrows
                // the stack's own shapes.
                if root.pending-key != "" && !root.scanning : Text {
                    text: root.pending-key;
                    color: Theme.muted;
                    font-size: 0.8125rem;
                    font-family: Theme.mono;
                    overflow: elide;
                }
                if root.pending-key != "" && !root.scanning : LineEdit {
                    placeholder-text: "Their name";
                    text <=> root.new-name;
                }
                if root.pending-key != "" && !root.scanning : Wide {
                    text: "Add";
                    primary: true;
                    enabled: root.new-name != "";
                    clicked => { root.add(root.new-name, root.pending-key); }
                }

                if root.pending-key == "" : Wide {
                    text: root.scanning ? "Stop scanning" : "Scan a code";
                    primary: !root.scanning;
                    clicked => { root.scan(!root.scanning); }
                }
                if root.pending-key == "" && !root.scanning : Wide {
                    text: "Choose an image";
                    clicked => { root.pick(); }
                }
                if root.pending-key == "" && !root.scanning : Wide {
                    text: "Share my key";
                    clicked => { root.share(); }
                }
            }
            // Whatever is left over ends up here rather than between the rows above.
            Rectangle { vertical-stretch: 1; }
        }
    }

    component SettingsPage inherits Rectangle {
        in property <length> top-inset;
        in property <string> quality;
        callback open-log();
        background: Theme.ground;

        VerticalLayout {
            HorizontalLayout {
                vertical-stretch: 0;
                padding-top: root.top-inset + Theme.bar-top;
                padding-left: Theme.edge;
                padding-right: Theme.edge;
                padding-bottom: Theme.bar-bottom;
                PageTitle { text: "Settings"; }
            }
            // A flat list like the design's, hairline above every row including the first.
            DetailRow {
                title: "Appearance";
                detail: Theme.appearance == Appearance.system ? "Follows the system" : "Set on this phone";
                value: Theme.appearance == Appearance.system ? "System"
                    : Theme.appearance == Appearance.light ? "Light"
                    : "Dark";
                clicked => {
                    Theme.appearance = Theme.appearance == Appearance.system ? Appearance.light
                        : Theme.appearance == Appearance.light ? Appearance.dark
                        : Appearance.system;
                }
            }
            DetailRow {
                title: "Layout direction";
                detail: "Mirrors every screen, for Arabic";
                value: Theme.rtl ? "Right to left" : "Left to right";
                clicked => { Theme.rtl = !Theme.rtl; }
            }
            DetailRow {
                title: "Call quality";
                detail: "Caps what the camera sends";
                value: root.quality;
                tappable: false;
            }
            DetailRow {
                title: "Diagnostics";
                detail: "What this run has been doing";
                value: "Open";
                clicked => { root.open-log(); }
            }
            Rectangle { vertical-stretch: 1; }
        }
    }

    // The whole window while a call is up: video owns the frame, chrome sits on a scrim.
    component CallScreen inherits Rectangle {
        in property <image> frame;
        in property <image> remote-frame;
        in property <string> peer-name;
        in property <string> peer-initial;
        in property <string> timer;
        in property <string> key-exchange;
        in property <string> status;
        in property <CallState> state;
        in-out property <bool> swapped;
        in property <bool> mic-on;
        in property <bool> speaker-on;
        in property <length> top-inset;
        in property <length> bottom-inset;
        callback accept();
        callback reject();
        callback hangup();
        callback toggle-mic();
        callback toggle-speaker();
        callback flip-camera();
        background: Theme.video;

        property <bool> connected: state == CallState.connected;

        Image {
            width: parent.width;
            height: parent.height;
            source: root.connected && !root.swapped ? root.remote-frame : root.frame;
            image-fit: cover;
        }

        // Self view: the whole frame until the call connects, then a corner card.
        self-view := Rectangle {
            property <bool> inset: root.connected && !root.swapped;
            x: inset ? (Theme.rtl ? Theme.self-view-edge : parent.width - self.width - Theme.self-view-edge) : 0px;
            y: inset ? root.top-inset + Theme.self-view-top : 0px;
            width: inset ? Theme.self-view-width : parent.width;
            height: inset ? Theme.self-view-height : parent.height;
            border-radius: inset ? Theme.self-view-edge : 0px;
            border-width: inset ? 1px : 0px;
            border-color: #FFFFFF2D;
            clip: true;
            visible: root.connected;
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

        // Ringing and incoming: name over a dimmed camera.
        if !root.connected : Rectangle {
            background: #070A0FB3;
            VerticalLayout {
                alignment: center;
                spacing: Theme.call-top;
                padding-bottom: parent.height / 4;
                HorizontalLayout {
                    alignment: center;
                    Avatar {
                        initial: root.peer-initial;
                        tint: 0;
                        size: Theme.peer-avatar;
                    }
                }
                Text {
                    text: root.peer-name;
                    color: Theme.on-video;
                    font-family: Theme.display;
                    font-size: 1.4375rem;
                    font-weight: 600;
                    height: self.font-size * Theme.line-box;
                    horizontal-alignment: center;
                    vertical-alignment: center;
                }
                Text {
                    text: root.state == CallState.incoming ? "Incoming video call" : "Calling…";
                    color: Theme.on-video-muted;
                    font-size: 0.9375rem;
                    height: self.font-size * Theme.line-box;
                    horizontal-alignment: center;
                    vertical-alignment: center;
                }
            }
        }

        // Top scrim: who, how long, how secure. Only once connected — while it is ringing the
        // name is already in the middle of the screen.
        if root.connected : Rectangle {
            y: 0;
            height: root.top-inset + 88px;
            background: @linear-gradient(180deg, #04080DBF 0%, #04080D00 100%);
            VerticalLayout {
                padding-top: root.top-inset + Theme.call-top;
                padding-left: Theme.call-edge;
                padding-right: Theme.call-edge;
                alignment: start;
                HorizontalLayout {
                    alignment: space-between;
                    spacing: Theme.gap-large;
                    VerticalLayout {
                        spacing: 3px;
                        Text {
                            text: root.peer-name;
                            color: Theme.on-video;
                            font-family: Theme.display;
                            font-size: 1.1875rem;
                            font-weight: 600;
                            height: self.font-size * Theme.line-box;
                            vertical-alignment: center;
                        }
                        Text {
                            text: root.connected ? root.timer : root.status;
                            color: Theme.on-video-muted;
                            font-size: 1rem;
                            font-family: root.connected ? Theme.mono : "";
                            height: self.font-size * Theme.line-box;
                            vertical-alignment: center;
                        }
                    }
                    if root.connected && root.key-exchange != "" : VerticalLayout {
                        alignment: start;
                        Rectangle {
                            height: Theme.chip-height;
                            border-radius: self.height / 2;
                            background: #58B6FF29;
                            border-width: 1px;
                            border-color: #58B6FF4C;
                            HorizontalLayout {
                                padding-left: Theme.chip-pad;
                                padding-right: Theme.chip-pad;
                                Text {
                                    text: root.key-exchange;
                                    color: Theme.on-video-accent;
                                    font-family: Theme.mono;
                                    font-size: 0.75rem;
                                    letter-spacing: 0.34px;
                                    vertical-alignment: center;
                                }
                            }
                        }
                    }
                }
            }
        }

        // Controls on their own scrim.
        Rectangle {
            y: parent.height - self.height;
            height: root.bottom-inset + 120px;
            background: @linear-gradient(0deg, #04080DD9 0%, #04080D00 100%);
            VerticalLayout {
                alignment: end;
                padding-bottom: root.bottom-inset + Theme.call-bottom;
                HorizontalLayout {
                    alignment: center;
                    spacing: Theme.key-gap;
                    if root.state == CallState.incoming : Key {
                        icon: @image-url("icons/close.svg");
                        label: "Decline";
                        danger: true;
                        clicked => { root.reject(); }
                    }
                    if root.state == CallState.incoming : Key {
                        icon: @image-url("icons/call.svg");
                        label: "Answer";
                        positive: true;
                        clicked => { root.accept(); }
                    }
                    if root.state != CallState.incoming : Key {
                        icon: root.mic-on ? @image-url("icons/mic.svg") : @image-url("icons/mic-off.svg");
                        label: root.mic-on ? "Mute" : "Unmute";
                        on: !root.mic-on;
                        clicked => { root.toggle-mic(); }
                    }
                    if root.connected : Key {
                        icon: @image-url("icons/speaker.svg");
                        label: "Speaker";
                        on: root.speaker-on;
                        clicked => { root.toggle-speaker(); }
                    }
                    if root.connected : Key {
                        icon: @image-url("icons/camera-switch.svg");
                        label: "Switch camera";
                        clicked => { root.flip-camera(); }
                    }
                    if root.state != CallState.incoming : Key {
                        icon: @image-url("icons/call-end.svg");
                        label: "End call";
                        danger: true;
                        clicked => { root.hangup(); }
                    }
                }
            }
        }
    }

    export component App inherits Window {
        in property <image> frame;
        in property <image> remote-frame;
        in property <image> qr;
        in property <string> my-id;
        in property <[string]> my-fingerprint-lines;
        in property <string> my-short-fingerprint;
        in property <string> stats;
        in property <string> log;
        in property <string> call-status;
        in property <string> peer-name;
        in property <string> peer-initial: "?";
        in property <string> call-timer;
        in property <string> key-exchange;
        in property <[ContactItem]> contacts;
        in property <string> quality: "720p · 30";

        in-out property <Screen> screen: Screen.people;
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
        callback share-key();
        callback scan(bool);

        title: "uplink";
        background: Theme.ground;
        // 1rem is Material 3's body-large, so the design's sizes read as their roles: 1.5rem
        // headline-small, 0.8125rem the 13px fingerprints. Android scales it with the system size.
        default-font-size: 16px;
        // Prose is Public Sans; Outfit and Plex Mono are asked for by the elements that want them.
        default-font-family: "Public Sans";
        // Our surfaces are dark, so the std-widgets follow rather than fight them.
        init => { Palette.color-scheme = ColorScheme.dark; }

        // Pages and the tab bar share the window: the bar takes space, it does not cover content.
        VerticalLayout {
            Rectangle {
                vertical-stretch: 1;
                clip: true;
                if root.screen == Screen.people : PeoplePage {
                    top-inset: root.safe-area-insets.top;
                    bottom-inset: root.safe-area-insets.bottom;
                    status: root.call-status;
                    contacts: root.contacts;
                    fingerprint: root.my-short-fingerprint;
                    call(id) => { root.call(id); }
                    add-someone => { root.screen = Screen.identity; }
                    show-key => { root.screen = Screen.identity; }
                }
                if root.screen == Screen.identity : KeyPage {
                    top-inset: root.safe-area-insets.top;
                    qr: root.qr;
                    frame: root.frame;
                    fingerprint-lines: root.my-fingerprint-lines;
                    scanning: root.scanning;
                    pending-key: root.peer-key;
                    new-name <=> root.new-name;
                    scan(on) => { root.scan(on); }
                    pick => { root.pick-key(); }
                    share => { root.share-key(); }
                    add(name, key) => { root.add-contact(name, key); }
                    close => { root.screen = Screen.people; }
                }
                if root.screen == Screen.settings : SettingsPage {
                    top-inset: root.safe-area-insets.top;
                    quality: root.quality;
                    open-log => { root.log-open = true; }
                }
            }
            // People ends with its own key row, as the design draws it, so the bar would be a
            // second footer. It stays on the screens that are not aligned to the design yet.
            if root.screen != Screen.people : Tabs {
                screen <=> root.screen;
                bottom-inset: root.safe-area-insets.bottom;
            }
        }

        // Overlays: declared last, so they cover the pages and the tab bar.
        if root.call-state != CallState.idle : CallScreen {
            frame: root.frame;
            remote-frame: root.remote-frame;
            peer-name: root.peer-name;
            peer-initial: root.peer-initial;
            timer: root.call-timer;
            key-exchange: root.key-exchange;
            status: root.call-status;
            state: root.call-state;
            swapped <=> root.swapped;
            mic-on: root.mic-on;
            speaker-on: root.speaker-on;
            top-inset: root.safe-area-insets.top;
            bottom-inset: root.safe-area-insets.bottom;
            accept => { root.accept(); }
            reject => { root.reject(); }
            hangup => { root.hangup(); }
            toggle-mic => { root.toggle-mic(); }
            toggle-speaker => { root.toggle-speaker(); }
            flip-camera => { root.flip-camera(); }
        }

        if root.log-open : Rectangle {
            background: #000000F2;
            VerticalBox {
                padding-top: root.safe-area-insets.top + Theme.gap-large;
                padding-bottom: root.safe-area-insets.bottom + Theme.gap-large;
                spacing: Theme.gap;
                HorizontalLayout {
                    alignment: space-between;
                    Text {
                        text: "Diagnostics";
                        color: Theme.text;
                        font-size: 1.3rem;
                        font-weight: 600;
                        vertical-alignment: center;
                    }
                    Button {
                        text: "Close";
                        clicked => { root.log-open = false; }
                    }
                }
                Card {
                    height: live.preferred-height;
                    live := VerticalBox {
                        spacing: 2px;
                        Text {
                            text: "Now";
                            color: Theme.text;
                            font-size: 0.9rem;
                            font-weight: 600;
                        }
                        Text {
                            text: root.stats;
                            color: Theme.muted;
                            font-size: 0.7rem;
                            font-family: Theme.mono;
                            wrap: word-wrap;
                        }
                    }
                }
                ScrollView {
                    vertical-stretch: 1;
                    Text {
                        text: root.log;
                        color: Theme.muted;
                        font-size: 0.7rem;
                        font-family: Theme.mono;
                        wrap: word-wrap;
                    }
                }
            }
        }
    }
}

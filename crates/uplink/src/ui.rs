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

    import { CheckBox, LineEdit, ScrollView, Palette } from "std-widgets.slint";

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
    // People is who you can call, Connect is how anyone becomes one of them, Settings is the rest.
    // Your own identity is not a contact, so it lives in Connect rather than in People.
    export enum Screen { people, calls, connect, settings }

    export struct CallItem {
        name: string,
        id: string,
        // "Missed · 20 minutes ago", and the mark that says which way it went.
        detail: string,
        initial: string,
        tint: int,
        missed: bool,
        incoming: bool,
        // Set when this row is the first of a day, so the list can date its groups.
        header: string,
    }
    export enum CallState { idle, dialing, ringing, incoming, connected }

    // A call cannot happen without all three, so the gate is not dismissable. `blocked` is the
    // one Android will not ask about again, which is the only case that needs Settings.
    export enum Grant { needed, granted, blocked }

    // What is waiting on a yes. `none` is the usual state.
    export enum Confirm { none, remove-contact, remove-selected, clear-calls }

    export struct PermissionItem {
        name: string,
        why: string,
        grant: Grant,
        icon: image,
    }

    export struct ContactItem {
        name: string,
        id: string,
        // When they were last called, and the heading of the group this row opens — set only on
        // the first row of each, so the markup never looks at the row before this one.
        detail: string,
        header: string,
        initial: string,
        tint: int,
        favourite: bool,
        selected: bool,
        // Just added: the row shimmers for a moment, so the eye lands on it.
        fresh: bool,
    }

    // A relay the endpoint may use. `host` is what settings store, not what is shown: two relays
    // in one region would read alike otherwise.
    export struct RelayItem {
        host: string,
        name: string,
        region: string,
        on: bool,
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
        out property <length> hairline-width: 1px;

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
        // The call once it has been folded away: big enough to read a face, small enough that the
        // screen underneath is still usable, which is the whole point of it.
        out property <length> mini-width: 108px;
        out property <length> mini-height: 158px;
        out property <length> mini-edge: 12px;
        // How far a finger may wander and still count as a tap. Android's own touch slop.
        out property <length> drag-slop: 8px;
        // How long a press has to be held to count as a long press. Android's default until the
        // app reads the user's own setting ("Touch and hold delay") from `ViewConfiguration`.
        in-out property <duration> long-press: 400ms;
        // The tab bar's own height, without the gesture inset it adds underneath. Named because
        // the folded call has to stay off it.
        out property <length> tab-height: 64px;
        out property <length> peer-avatar: 110px;
        // The Add-someone stack.
        out property <length> wide-height: 44px;
        out property <length> wide-radius: 15px;
        out property <length> stack-gap: 12px;
        out property <length> stack-top: 17px;
        out property <length> qr-size: 171px;
        out property <length> qr-pad: 12px;
        // The mark on its plate in the middle of a code: the plate is round, so what fits inside
        // it is the square on its diameter — 1/√2 of the width, not all of it.
        out property <float> mark-in-plate: 0.707;
        // The splash, scaled from the design's frame like everything else.
        out property <length> splash-mark: 105px;
        out property <length> splash-gap: 20px;
        out property <length> splash-foot: 32px;
        out property <length> group-head: 30px;
        // The copy-key button: a full touch target around a key-sized icon, and how long its
        // check mark stays before it offers to copy again.
        out property <length> copy-target: 48px;
        out property <duration> copied-for: 1500ms;
        // A just-added contact: a faint tint with a band of accent sweeping across it.
        out property <duration> shimmer-sweep: 1400ms;
        out property <float> shimmer-band: 0.45;
        out property <color> shimmer-tint: beacon.with-alpha(0.08);
        out property <color> shimmer-glint: beacon.with-alpha(0.22);
        // Settings: rows on inset surface groups, each with an icon tile. Drawn at 360dp in the
        // settings canvas, so these are dp as they stand.
        out property <length> setting-height: 68px;
        out property <length> setting-pad: 16px;
        out property <length> setting-gap: 14px;
        out property <length> setting-tile: 38px;
        out property <length> setting-tile-radius: 11px;
        out property <length> setting-icon: 20px;
        out property <length> setting-title-box: 24px;
        out property <length> setting-detail-box: 19px;
        out property <length> group-edge: 16px;
        // Stronger than `hairline`: on a group's surface the page's own reads as nothing.
        out property <color> group-hairline: dark ? #263140E6 : #E1E7EE;
        out property <length> group-gap: 14px;
        out property <length> brand: 26px;
        out property <length> sheet-width: 320px;
        out property <length> toast-bottom: 88px;
        // Long enough to read a sentence, short enough not to sit in the way.
        out property <duration> toast-life: 4s;
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

    // The mark, small, at the end of every app bar — so whichever screen someone is looking at
    // still says whose app it is.
    component Brand inherits VerticalLayout {
        alignment: center;
        Image {
            source: Theme.dark ? @image-url("icons/mark.svg") : @image-url("icons/mark-light.svg");
            width: Theme.brand;
            height: self.width;
            accessible-role: image;
            accessible-label: "uplink";
        }
    }

    component PageTitle inherits Text {
        color: Theme.text;
        font-family: Theme.display;
        font-size: 1.4375rem;
        font-weight: 600;
        height: self.font-size * Theme.line-box;
        vertical-alignment: center;
    }

    // The design's full-width action. Its secondary colours were written for a dark ground, so
    // here they come from the accent rather than the literal pale blue, to stay readable in light.
    component Wide inherits Rectangle {
        in property <string> text;
        in property <bool> primary: false;
        in property <bool> danger: false;
        in property <bool> enabled: true;
        callback clicked();
        property <color> accent: root.danger ? Theme.end : Theme.beacon;
        height: Theme.wide-height;
        border-radius: Theme.wide-radius;
        background: root.primary ? Theme.beacon : self.accent.with-alpha(0.12);
        border-width: root.primary ? 0px : 1px;
        border-color: self.accent.with-alpha(0.30);
        opacity: root.enabled ? 1.0 : 0.45;
        accessible-role: button;
        accessible-label: text;
        accessible-action-default => { root.clicked(); }
        Text {
            text: root.text;
            color: root.primary ? (Theme.dark ? #04121F : #FFFFFF) : root.accent;
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

    // The first thing a launch shows, over the endpoint coming up. It follows the theme: a dark
    // splash handing over to a light app is a flash, and the ground is the one thing that must
    // not change across the handover.
    component Splash inherits Rectangle {
        background: Theme.ground;
        VerticalLayout {
            alignment: center;
            spacing: Theme.splash-gap;
            HorizontalLayout {
                alignment: center;
                Image {
                    // Two files rather than one tinted: the mark is two-tone, and `colorize`
                    // would flatten the peers into the link.
                    source: Theme.dark ? @image-url("icons/mark.svg") : @image-url("icons/mark-light.svg");
                    width: Theme.splash-mark;
                    height: self.width;
                }
            }
            Text {
                text: "uplink";
                color: Theme.text;
                font-family: Theme.display;
                font-size: 1.90625rem;
                font-weight: 600;
                height: self.font-size * Theme.line-box;
                horizontal-alignment: center;
                vertical-alignment: center;
            }
        }
        Text {
            y: parent.height - self.height - Theme.splash-foot;
            width: parent.width;
            text: "P2P · E2EE";
            color: Theme.muted;
            font-family: Theme.mono;
            font-size: 0.8125rem;
            letter-spacing: 1.8px;
            horizontal-alignment: center;
        }
    }

    // The gate. Everything a call needs, asked for once, with the reason next to each — asking
    // cold is how people tap Deny.
    component PermissionsPage inherits Rectangle {
        in property <[PermissionItem]> permissions;
        in property <bool> blocked;
        in property <length> top-inset;
        callback grant();
        callback settings();
        background: Theme.ground;

        VerticalLayout {
            HorizontalLayout {
                vertical-stretch: 0;
                padding-top: root.top-inset + Theme.bar-top;
                padding-left: Theme.edge;
                padding-right: Theme.edge;
                padding-bottom: Theme.bar-bottom;
                PageTitle { text: root.blocked ? "uplink can't call yet" : "Before your first call"; }
            }
            VerticalLayout {
                padding-left: Theme.edge;
                padding-right: Theme.edge;
                spacing: Theme.stack-gap;
                Text {
                    text: root.blocked
                        ? "Android won't ask again, so these have to be turned on in settings. uplink will be waiting when you come back."
                        : "A call needs all three. uplink asks once, and asks for nothing else — no contacts, no location, no storage.";
                    color: Theme.muted;
                    font-size: 0.9375rem;
                    wrap: word-wrap;
                }
                for item in root.permissions : HorizontalLayout {
                    spacing: Theme.row-gap;
                    VerticalLayout {
                        alignment: center;
                        Image {
                            source: item.icon;
                            width: Theme.key-icon;
                            height: self.width;
                            colorize: item.grant == Grant.blocked ? Theme.end
                                : item.grant == Grant.granted ? Theme.answer
                                : Theme.beacon;
                        }
                    }
                    VerticalLayout {
                        alignment: center;
                        horizontal-stretch: 1;
                        Text {
                            text: item.name;
                            color: Theme.text;
                            font-size: 1rem;
                            font-weight: 600;
                            height: self.font-size * Theme.line-box;
                            vertical-alignment: center;
                        }
                        Text {
                            text: item.why;
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
                            text: item.grant == Grant.granted ? "on" : item.grant == Grant.blocked ? "off" : "needed";
                            color: item.grant == Grant.granted ? Theme.answer
                                : item.grant == Grant.blocked ? Theme.end
                                : Theme.muted;
                            font-family: Theme.mono;
                            font-size: 0.75rem;
                            letter-spacing: 0.6px;
                        }
                    }
                }
                Wide {
                    text: root.blocked ? "Open settings" : "Continue";
                    primary: true;
                    clicked => {
                        if (root.blocked) {
                            root.settings();
                        } else {
                            root.grant();
                        }
                    }
                }
            }
            Rectangle { vertical-stretch: 1; }
        }
    }

    // After the gate, once: the battery exemption, explained before Android's dialog the way the
    // gate explains permissions. Unlike the gate it can be declined — uplink works without it,
    // it just may not ring overnight — so "Not now" is as easy to reach as "Allow".
    component BatteryPage inherits Rectangle {
        in property <length> top-inset;
        callback allow();
        callback skip();
        background: Theme.ground;

        VerticalLayout {
            HorizontalLayout {
                vertical-stretch: 0;
                padding-top: root.top-inset + Theme.bar-top;
                padding-left: Theme.edge;
                padding-right: Theme.edge;
                padding-bottom: Theme.bar-bottom;
                PageTitle { text: "Stay reachable"; }
            }
            VerticalLayout {
                padding-left: Theme.edge;
                padding-right: Theme.edge;
                spacing: Theme.stack-gap;
                Text {
                    text: "To save battery, Android stops apps it thinks nobody is using. uplink has to keep listening, or a call can't reach you while the phone sleeps.";
                    color: Theme.muted;
                    font-size: 0.9375rem;
                    wrap: word-wrap;
                }
                HorizontalLayout {
                    spacing: Theme.row-gap;
                    VerticalLayout {
                        alignment: center;
                        Image {
                            source: @image-url("icons/call.svg");
                            width: Theme.key-icon;
                            height: self.width;
                            colorize: Theme.beacon;
                        }
                    }
                    VerticalLayout {
                        alignment: center;
                        horizontal-stretch: 1;
                        Text {
                            text: "Calls while the phone sleeps";
                            color: Theme.text;
                            font-size: 1rem;
                            font-weight: 600;
                            height: self.font-size * Theme.line-box;
                            vertical-alignment: center;
                        }
                        Text {
                            text: "Android asks next. You can change it later in Settings.";
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
                            text: "optional";
                            color: Theme.muted;
                            font-family: Theme.mono;
                            font-size: 0.75rem;
                            letter-spacing: 0.6px;
                        }
                    }
                }
                Wide {
                    text: "Allow";
                    primary: true;
                    clicked => { root.allow(); }
                }
                Wide {
                    text: "Not now";
                    clicked => { root.skip(); }
                }
            }
            Rectangle { vertical-stretch: 1; }
        }
    }

    component Tabs inherits Rectangle {
        in-out property <Screen> screen;
        // The gesture bar sits under the tabs, so the row clears it from the inside.
        in property <length> bottom-inset;
        height: Theme.tab-height + bottom-inset;
        background: Theme.surface;
        HorizontalLayout {
            padding: 6px;
            padding-bottom: 6px + root.bottom-inset;
            spacing: 4px;
            for tab in [
                { label: "People", screen: Screen.people },
                { label: "Calls", screen: Screen.calls },
                { label: "Connect", screen: Screen.connect },
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
        in property <bool> danger: false;
        in property <bool> enabled: true;
        callback clicked();
        property <color> accent: root.danger ? Theme.end : Theme.beacon;
        height: Theme.pill-height;
        width: label.preferred-width + 28px;
        border-radius: self.height / 2;
        background: transparent;
        border-width: 1px;
        border-color: root.accent.with-alpha(0.35);
        opacity: root.enabled ? 1.0 : 0.4;
        accessible-role: button;
        accessible-label: text;
        accessible-action-default => { root.clicked(); }
        label := Text {
            text: root.text;
            color: root.accent;
            font-size: 0.8125rem;
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

    // What a thing is on the start side, what it is set to on the end side, on a flat list with a
    // hairline above each row. The contact screen's rows; Settings has its own, in groups.
    component DetailRow inherits Rectangle {
        in property <string> title;
        in property <string> detail;
        in property <string> value;
        // Shown in place of `value`, because a glyph like a cross or a star is not in every face
        // and a missing one renders as nothing at all.
        in property <image> icon;
        in property <bool> danger: false;
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
                    color: root.danger ? Theme.end : Theme.text;
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
                if root.icon.width > 0 : Image {
                    source: root.icon;
                    width: Theme.key-icon;
                    height: self.width;
                    colorize: root.danger ? Theme.end : Theme.beacon;
                }
                if root.icon.width <= 0 : Text {
                    text: root.value;
                    color: root.danger ? Theme.end : root.tappable ? Theme.beacon : Theme.muted;
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
        // Offset of a row to bring into view, from the top of the list; negative for none.
        in property <length> reveal: -1px;
        in property <length> top-inset;
        in property <bool> online;
        in property <bool> selecting;
        in property <int> selected-count;
        callback call(string);
        callback open(string);
        callback connect();
        callback select(string);
        // A row was held long enough to count; the feel of it is the platform's.
        callback long-pressed();
        callback start-selecting();
        callback remove-selected();
        background: Theme.ground;

        VerticalLayout {
            // Contacts and nothing else: adding happens in Connect, which is a tab away.
            HorizontalLayout {
                vertical-stretch: 0;
                padding-top: root.top-inset + Theme.bar-top;
                padding-left: Theme.edge;
                padding-right: Theme.edge;
                padding-bottom: Theme.bar-bottom;
                PageTitle {
                    text: root.selecting ? root.selected-count + " selected" : "People";
                    horizontal-stretch: 1;
                    vertical-alignment: center;
                }
                if root.selecting : VerticalLayout {
                    alignment: center;
                    Pill {
                        text: "Remove";
                        danger: true;
                        enabled: root.selected-count > 0;
                        clicked => { root.remove-selected(); }
                    }
                }
                // Reachability, not diagnostics: whether someone could call you right now.
                if !root.selecting : VerticalLayout {
                    alignment: center;
                    Rectangle {
                        height: Theme.pill-height;
                        width: reach.preferred-width + 28px;
                        border-radius: self.height / 2;
                        background: transparent;
                        border-width: 1px;
                        border-color: root.online ? Theme.answer.with-alpha(0.45) : Theme.muted.with-alpha(0.45);
                        reach := Text {
                            text: root.online ? "Online" : "Offline";
                            color: root.online ? Theme.answer : Theme.muted;
                            font-size: 0.8125rem;
                        }
                    }
                }
                Brand { }
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
                        text: "Open Connect to scan a code, or send them yours.";
                        color: Theme.muted;
                        font-size: 0.875rem;
                        horizontal-alignment: center;
                        wrap: word-wrap;
                    }
                    HorizontalLayout {
                        alignment: center;
                        Pill {
                            text: "Connect";
                            clicked => { root.connect(); }
                        }
                    }
                }
            }
            if root.contacts.length > 0 : scroller := ScrollView {
                vertical-stretch: 1;
                // A finger drags the list itself; without this only the scrollbar moves it.
                mouse-drag-pan-enabled: true;
                // A row asked to be shown is centred when it can be, and the list stops at its
                // ends when it cannot. Both on arrival and on a change while the page is up.
                property <length> reveal: root.reveal;
                function show-revealed() {
                    if (self.reveal >= 0) {
                        self.content-y = -clamp(
                            self.reveal - (self.visible-height - Theme.row-height) / 2,
                            0px,
                            max(0px, self.content-height - self.visible-height));
                    }
                }
                // Also once the layout has measured the rows: at `init` the content has no height
                // yet, so the clamp above pins the list to the top and the reveal goes nowhere.
                init => { self.show-revealed(); }
                changed reveal => { self.show-revealed(); }
                changed content-height => { self.show-revealed(); }
                changed visible-height => { self.show-revealed(); }
                VerticalLayout {
                    alignment: start;
                    // A flat list: rows are divided by a hairline, never boxed. The hairline is
                    // part of the stack, as the design's border-top is, so the pitch includes it.
                    for contact[index] in root.contacts : VerticalLayout {
                        if contact.header != "" : HorizontalLayout {
                            padding-left: Theme.edge;
                            padding-right: Theme.edge;
                            Text {
                                text: contact.header;
                                color: Theme.muted;
                                font-family: Theme.mono;
                                font-size: 0.6875rem;
                                letter-spacing: 1.6px;
                                height: Theme.group-head;
                                vertical-alignment: bottom;
                            }
                        }
                        // A hairline divides rows within a group; a heading already divides groups.
                        if index > 0 && contact.header == "" : Rectangle {
                            // Rust counts this too, to know where a row sits.
                            height: Theme.hairline-width;
                            background: Theme.hairline;
                        }
                        Rectangle {
                            height: Theme.row-height;
                            // The pill calls; the rest of the row opens them, because a contact
                            // now has more to do to it than one button can carry. Holding a row
                            // starts selecting with it, which is the only way into selection.
                            property <bool> holding: false;
                            // The release that ends a long press is not also a tap.
                            property <bool> long-pressed: false;
                            open-touch := TouchArea {
                                mouse-cursor: pointer;
                                pointer-event(event) => {
                                    if (event.kind == PointerEventKind.down) {
                                        parent.long-pressed = false;
                                        parent.holding = true;
                                    } else if (event.kind == PointerEventKind.up || event.kind == PointerEventKind.cancel) {
                                        parent.holding = false;
                                    }
                                }
                                // A finger that wanders is scrolling the list, not holding a row.
                                moved => {
                                    if (abs(self.mouse-x - self.pressed-x) > Theme.drag-slop
                                        || abs(self.mouse-y - self.pressed-y) > Theme.drag-slop) {
                                        parent.holding = false;
                                    }
                                }
                                clicked => {
                                    if (parent.long-pressed) {
                                        return;
                                    }
                                    if (root.selecting) {
                                        root.select(contact.id);
                                    } else {
                                        root.open(contact.id);
                                    }
                                }
                            }
                            Timer {
                                interval: Theme.long-press;
                                running: parent.holding;
                                triggered => {
                                    parent.holding = false;
                                    parent.long-pressed = true;
                                    root.long-pressed();
                                    if (!root.selecting) {
                                        root.start-selecting();
                                    }
                                    root.select(contact.id);
                                }
                            }
                            background: contact.selected ? Theme.raised
                                : open-touch.pressed ? Theme.surface
                                : transparent;
                            animate background { duration: 160ms; easing: ease-out; }
                            accessible-role: button;
                            accessible-label: contact.name;
                            accessible-action-default => { root.open(contact.id); }
                            // Under the row's contents, over its background.
                            if contact.fresh : Rectangle {
                                clip: true;
                                background: Theme.shimmer-tint;
                                // 0 to 1 across each sweep, for as long as the row is fresh.
                                property <float> phase: mod(animation-tick() / 1ms, Theme.shimmer-sweep / 1ms)
                                    / (Theme.shimmer-sweep / 1ms);
                                Rectangle {
                                    width: parent.width * Theme.shimmer-band;
                                    x: -self.width + (parent.width + self.width) * parent.phase;
                                    background: @linear-gradient(90deg,
                                        Theme.shimmer-glint.with-alpha(0) 0%,
                                        Theme.shimmer-glint 50%,
                                        Theme.shimmer-glint.with-alpha(0) 100%);
                                }
                            }
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
                                        text: contact.detail;
                                        color: Theme.muted;
                                        // When they were last called is prose, not a code, so it
                                        // takes the body face rather than the monospace one.
                                        font-size: 0.8125rem;
                                        height: self.font-size * Theme.line-box;
                                        vertical-alignment: center;
                                        overflow: elide;
                                    }
                                }
                                VerticalLayout {
                                    alignment: center;
                                    if !root.selecting : Pill {
                                        text: "Call";
                                        clicked => { root.call(contact.id); }
                                    }
                                    if root.selecting : Image {
                                        source: contact.selected
                                            ? @image-url("icons/check-on.svg")
                                            : @image-url("icons/check-off.svg");
                                        width: Theme.key-icon;
                                        height: self.width;
                                        colorize: contact.selected ? Theme.beacon : Theme.muted;
                                    }
                                }
                            }
                        }
                    }
                }
            }

        }
    }

    // Anything that cannot be undone asks first. A tap is cheap and a key may be gone for good.
    component ConfirmSheet inherits Rectangle {
        in property <string> title;
        in property <string> body;
        in property <string> confirm-label;
        callback confirm();
        callback cancel();
        background: #000000CC;

        // Swallows taps on the dimmed area, so the sheet is dismissed deliberately.
        TouchArea { }

        Rectangle {
            width: min(parent.width - Theme.edge * 2, Theme.sheet-width);
            height: body-layout.preferred-height;
            border-radius: Theme.wide-radius;
            background: Theme.ground;
            body-layout := VerticalLayout {
                padding: Theme.edge;
                spacing: Theme.stack-gap;
                Text {
                    text: root.title;
                    color: Theme.text;
                    font-family: Theme.display;
                    font-size: 1.1875rem;
                    font-weight: 600;
                    height: self.font-size * Theme.line-box;
                    vertical-alignment: center;
                    overflow: elide;
                }
                Text {
                    text: root.body;
                    color: Theme.muted;
                    font-size: 0.8125rem;
                    wrap: word-wrap;
                }
                Wide {
                    text: root.confirm-label;
                    danger: true;
                    clicked => { root.confirm(); }
                }
                Wide {
                    text: "Keep";
                    clicked => { root.cancel(); }
                }
            }
        }
    }

    // Naming a key that has just arrived — scanned, opened from an image, or pasted. A sheet over
    // whatever screen you were on, because a key can arrive while you are anywhere and the one
    // thing left to do should not be somewhere else.
    component NameSheet inherits Rectangle {
        in property <string> key;
        in property <length> keyboard;
        in-out property <string> name;
        // Why the last Add did not take, said under the field it concerns. Cleared by editing.
        in-out property <string> error;
        callback add(string);
        callback cancel();
        background: #000000CC;

        // Swallows taps on the dimmed area so nothing behind the sheet reacts.
        TouchArea { }

        Rectangle {
            width: min(parent.width - Theme.edge * 2, Theme.sheet-width);
            height: body.preferred-height;
            // Centred in what the keyboard leaves, not in the window, so the field the sheet
            // exists for is never the part that ends up underneath it.
            y: max(Theme.edge, (parent.height - root.keyboard - self.height) / 2);
            animate y { duration: 180ms; easing: ease-out; }
            border-radius: Theme.wide-radius;
            background: Theme.ground;
            body := VerticalLayout {
                padding: Theme.edge;
                spacing: Theme.stack-gap;
                Text {
                    text: "Name this contact";
                    color: Theme.text;
                    font-family: Theme.display;
                    font-size: 1.1875rem;
                    font-weight: 600;
                    height: self.font-size * Theme.line-box;
                    vertical-alignment: center;
                }
                Text {
                    text: "Only you see this name. They are saved by their key either way.";
                    color: Theme.muted;
                    font-size: 0.8125rem;
                    wrap: word-wrap;
                }
                Text {
                    text: root.key;
                    color: Theme.muted;
                    font-family: Theme.mono;
                    font-size: 0.75rem;
                    wrap: word-wrap;
                }
                LineEdit {
                    placeholder-text: "Their name";
                    text <=> root.name;
                    edited => { root.error = ""; }
                    accepted => {
                        if (root.name != "") {
                            root.add(root.name);
                        }
                    }
                }
                if root.error != "" : Text {
                    text: root.error;
                    color: Theme.end;
                    font-size: 0.8125rem;
                    wrap: word-wrap;
                    accessible-role: text;
                    accessible-label: root.error;
                }
                Wide {
                    text: "Add";
                    primary: true;
                    enabled: root.name != "";
                    clicked => { root.add(root.name); }
                }
                Wide {
                    text: "Not now";
                    clicked => { root.cancel(); }
                }
            }
        }
    }

    // Which relays the endpoint may use. A sheet rather than rows on the settings page, because
    // these are edited as a set: ticking three boxes one at a time would rebind the endpoint
    // three times, and two of those states are ones nobody asked for.
    // One relay in the sheet: what it is called, where it is, and the one thing you can do to it.
    component RelayRow inherits HorizontalLayout {
        in property <RelayItem> relay;
        in property <bool> removable: false;
        in-out property <bool> on;
        callback remove();
        spacing: Theme.value-gap;
        // Fixed, because the list's height is counted in rows rather than measured. A scroll
        // view has no preferred height of its own to give the sheet.
        height: Theme.detail-height;
        if !root.removable : CheckBox {
            checked: root.on;
            toggled => { root.on = self.checked; }
        }
        VerticalLayout {
            alignment: center;
            horizontal-stretch: 1;
            Text {
                text: root.relay.name;
                color: Theme.text;
                font-size: 1rem;
                font-weight: 700;
                height: self.font-size * Theme.line-box;
                vertical-alignment: center;
                overflow: elide;
            }
            Text {
                text: root.relay.region;
                color: Theme.muted;
                font-size: 0.8125rem;
                height: self.font-size * Theme.line-box;
                vertical-alignment: center;
                overflow: elide;
            }
        }
        if root.removable : VerticalLayout {
            alignment: center;
            Text {
                text: "Remove";
                color: Theme.end;
                font-size: 0.8125rem;
                font-weight: 700;
                vertical-alignment: center;
                accessible-role: button;
                accessible-label: "Remove " + root.relay.name;
                accessible-action-default => { root.remove(); }
                TouchArea {
                    mouse-cursor: pointer;
                    clicked => { root.remove(); }
                }
            }
        }
    }

    component RelaySheet inherits Rectangle {
        in property <[RelayItem]> relays;
        in property <[RelayItem]> custom-relays;
        in-out property <bool> custom;
        in-out property <string> new-name;
        in-out property <string> new-url;
        in property <length> keyboard;
        callback save();
        callback cancel();
        callback add(string, string);
        callback remove(string);
        background: #000000CC;

        // Swallows taps on the dimmed area so nothing behind the sheet reacts.
        TouchArea { }

        Rectangle {
            width: min(parent.width - Theme.edge * 2, Theme.sheet-width);
            height: min(body.preferred-height, parent.height - root.keyboard - Theme.edge * 2);
            y: max(Theme.edge, (parent.height - root.keyboard - self.height) / 2);
            animate y { duration: 180ms; easing: ease-out; }
            border-radius: Theme.wide-radius;
            background: Theme.ground;
            body := VerticalLayout {
                padding: Theme.edge;
                spacing: Theme.stack-gap;
                Text {
                    text: "Relays";
                    color: Theme.text;
                    font-family: Theme.display;
                    font-size: 1.1875rem;
                    font-weight: 600;
                    height: self.font-size * Theme.line-box;
                    vertical-alignment: center;
                }
                Text {
                    text: "Each one is contacted every few seconds, call or no call. Using only the ones near you costs nothing and saves battery and mobile data.";
                    color: Theme.muted;
                    font-size: 0.8125rem;
                    wrap: word-wrap;
                }
                // Which set is live. Both are listed whichever it is, because you pick the relay
                // you are moving to before you move to it.
                HorizontalLayout {
                    spacing: Theme.value-gap;
                    Wide {
                        text: "n0's";
                        primary: !root.custom;
                        clicked => { root.custom = false; }
                    }
                    Wide {
                        text: "My own";
                        primary: root.custom;
                        enabled: root.custom-relays.length > 0;
                        clicked => { root.custom = true; }
                    }
                }
                // Counted rather than measured: a ScrollView reports no preferred height of its
                // own, so a sheet sized to its content would collapse the list to a single row.
                ScrollView {
                    height: min(
                        (root.custom ? root.custom-relays.length : root.relays.length) * Theme.detail-height,
                        root.height / 3);
                    mouse-drag-pan-enabled: true;
                    VerticalLayout {
                        alignment: start;
                        if !root.custom : VerticalLayout {
                            for relay in root.relays : RelayRow {
                                relay: relay;
                                on <=> relay.on;
                            }
                        }
                        if root.custom : VerticalLayout {
                            for relay in root.custom-relays : RelayRow {
                                relay: relay;
                                removable: true;
                                remove => { root.remove(relay.host); }
                            }
                        }
                    }
                }
                if root.custom-relays.length == 0 && root.custom : Text {
                    text: "Add one below to use your own.";
                    color: Theme.muted;
                    font-size: 0.8125rem;
                    wrap: word-wrap;
                }
                LineEdit {
                    placeholder-text: "Name, e.g. Home";
                    text <=> root.new-name;
                }
                LineEdit {
                    placeholder-text: "https://relay.example.com";
                    input-type: text;
                    text <=> root.new-url;
                    accepted => {
                        if (root.new-url != "") {
                            root.add(root.new-name, root.new-url);
                        }
                    }
                }
                Wide {
                    text: "Add this relay";
                    enabled: root.new-url != "";
                    clicked => { root.add(root.new-name, root.new-url); }
                }
                Wide {
                    text: "Save";
                    primary: true;
                    clicked => { root.save(); }
                }
                Wide {
                    text: "Cancel";
                    clicked => { root.cancel(); }
                }
            }
        }
    }

    // One contact: what you call them, what they call themselves, and the three things you can do
    // to the row besides ring it.
    component ContactPage inherits Rectangle {
        in property <string> name;
        in property <string> advertised;
        in property <string> initial;
        in property <bool> favourite;
        in property <[string]> fingerprint-lines;
        in property <length> top-inset;
        in property <length> bottom-inset;
        in-out property <string> draft;
        in-out property <bool> renaming;
        callback call();
        callback toggle-favourite();
        callback rename(string);
        callback remove();
        callback close();
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
                    text: root.name;
                    horizontal-stretch: 1;
                    overflow: elide;
                }
                VerticalLayout {
                    alignment: center;
                    Pill {
                        text: "Done";
                        clicked => { root.close(); }
                    }
                }
            }

            // Everything below the bar scrolls: when the keyboard covers the rename field, Slint
            // looks up the parent chain for something scrollable and brings the field back into
            // view. Without it the field just sits underneath the keyboard.
            ScrollView {
                vertical-stretch: 1;
                mouse-drag-pan-enabled: true;
                VerticalLayout {
                    alignment: start;
                    padding-bottom: root.bottom-inset + Theme.gap-large;
                    VerticalLayout {
                        padding-left: Theme.edge;
                        padding-right: Theme.edge;
                        spacing: Theme.stack-gap;
                        HorizontalLayout {
                            alignment: center;
                            Avatar {
                                initial: root.initial;
                                tint: 0;
                                size: Theme.peer-avatar;
                            }
                        }
                        // Their claim sits under your name for them, in quotes, and never replaces it.
                        if root.advertised != "" : Text {
                            text: "calls themselves “" + root.advertised + "”";
                            color: Theme.muted;
                            font-size: 0.8125rem;
                            height: self.font-size * Theme.line-box;
                            horizontal-alignment: center;
                            vertical-alignment: center;
                            overflow: elide;
                        }
                        VerticalLayout {
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
                        if !root.renaming : Wide {
                            text: "Call";
                            primary: true;
                            clicked => { root.call(); }
                        }
                        if root.renaming : LineEdit {
                            placeholder-text: "Their name";
                            text <=> root.draft;
                        }
                        if root.renaming : Wide {
                            text: "Save";
                            primary: true;
                            enabled: root.draft != "" && root.draft != root.name;
                            clicked => {
                                root.rename(root.draft);
                                root.renaming = false;
                            }
                        }
                    }

                    // The actions sit clear of the Call button rather than butting against it.
                    Rectangle { height: Theme.stack-top; }
                    DetailRow {
                        title: "Favourite";
                        detail: root.favourite ? "Kept at the top of People" : "Keep them at the top of People";
                        icon: root.favourite ? @image-url("icons/star-filled.svg") : @image-url("icons/star.svg");
                        clicked => { root.toggle-favourite(); }
                    }
                    DetailRow {
                        title: "Rename";
                        detail: "What you call them, only on this phone";
                        value: root.renaming ? "Cancel" : "Edit";
                        clicked => {
                            root.draft = root.name;
                            root.renaming = !root.renaming;
                        }
                    }
                    DetailRow {
                        title: "Remove";
                        detail: "Their key goes; they can still call you";
                        icon: @image-url("icons/close.svg");
                        danger: true;
                        clicked => { root.remove(); }
                    }
                }
            }
        }
    }

    // What happened, and with whom. A missed call is the only thing here worth a colour.
    component CallsPage inherits Rectangle {
        in property <[CallItem]> calls;
        in property <length> top-inset;
        callback call(string);
        callback clear();
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
                    text: "Calls";
                    horizontal-stretch: 1;
                    vertical-alignment: center;
                }
                if root.calls.length > 0 : VerticalLayout {
                    alignment: center;
                    Pill {
                        text: "Clear";
                        danger: true;
                        clicked => { root.clear(); }
                    }
                }
                Brand { }
            }

            if root.calls.length == 0 : Rectangle {
                vertical-stretch: 1;
                VerticalLayout {
                    alignment: center;
                    spacing: Theme.gap;
                    padding-left: Theme.edge;
                    padding-right: Theme.edge;
                    Text {
                        text: "No calls yet";
                        color: Theme.text;
                        font-size: 1.0625rem;
                        horizontal-alignment: center;
                    }
                    Text {
                        text: "Calls you make and calls you miss both end up here.";
                        color: Theme.muted;
                        font-size: 0.875rem;
                        horizontal-alignment: center;
                        wrap: word-wrap;
                    }
                }
            }
            if root.calls.length > 0 : ScrollView {
                vertical-stretch: 1;
                mouse-drag-pan-enabled: true;
                VerticalLayout {
                    alignment: start;
                    for entry[index] in root.calls : VerticalLayout {
                        if entry.header != "" : HorizontalLayout {
                            padding-left: Theme.edge;
                            padding-right: Theme.edge;
                            Text {
                                text: entry.header;
                                color: Theme.muted;
                                font-family: Theme.mono;
                                font-size: 0.6875rem;
                                letter-spacing: 1.6px;
                                height: Theme.group-head;
                                vertical-alignment: bottom;
                            }
                        }
                        if index > 0 && entry.header == "" : Rectangle {
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
                                        initial: entry.initial;
                                        tint: entry.tint;
                                        size: Theme.avatar;
                                    }
                                }
                                VerticalLayout {
                                    alignment: center;
                                    spacing: 1px;
                                    horizontal-stretch: 1;
                                    Text {
                                        text: entry.name;
                                        color: entry.missed ? Theme.end : Theme.text;
                                        font-family: Theme.display;
                                        font-size: 1.0625rem;
                                        font-weight: 600;
                                        height: self.font-size * Theme.line-box;
                                        vertical-alignment: center;
                                        overflow: elide;
                                    }
                                    Text {
                                        text: (entry.incoming ? "↓ " : "↑ ") + entry.detail;
                                        color: Theme.muted;
                                        font-size: 0.8125rem;
                                        height: self.font-size * Theme.line-box;
                                        vertical-alignment: center;
                                        overflow: elide;
                                    }
                                }
                                VerticalLayout {
                                    alignment: center;
                                    Pill {
                                        text: "Call";
                                        clicked => { root.call(entry.id); }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    component KeyPage inherits Rectangle {
        in property <length> top-inset;
        in property <image> qr;
        in property <float> mark;
        in property <image> frame;
        in property <[string]> fingerprint-lines;
        in property <bool> scanning;
        in property <string> pending-key;
        in-out property <string> new-name;
        callback scan(bool);
        callback pick();
        callback share();
        callback copy();
        callback add(string, string);
        background: Theme.ground;

        // Shows the check for a moment after a copy, then goes back to offering another.
        property <bool> copied: false;
        function copy-key() {
            root.copy();
            root.copied = true;
        }
        Timer {
            interval: Theme.copied-for;
            running: root.copied;
            triggered => { root.copied = false; }
        }

        VerticalLayout {
            HorizontalLayout {
                vertical-stretch: 0;
                padding-top: root.top-inset + Theme.bar-top;
                padding-left: Theme.edge;
                padding-right: Theme.edge;
                padding-bottom: Theme.bar-bottom;
                spacing: Theme.gap;
                PageTitle {
                    text: root.scanning ? "Point at their code" : "Connect";
                    horizontal-stretch: 1;
                }
                if root.scanning : VerticalLayout {
                    alignment: center;
                    Pill {
                        text: "Stop";
                        clicked => { root.scan(false); }
                    }
                }
                Brand { }
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
                        code := Image {
                            width: parent.width - (root.scanning ? 0px : Theme.qr-pad * 2);
                            height: self.width;
                            source: root.scanning ? root.frame : root.qr;
                            image-fit: root.scanning ? ImageFit.cover : ImageFit.contain;
                        }
                        // The mark in the middle, the way a payment app puts its own there. The
                        // code is drawn with the error correction to lose it: `mark` is the share
                        // of its width uplink-core says can go, and the plate stays inside that.
                        if !root.scanning : Rectangle {
                            width: code.width * root.mark;
                            height: self.width;
                            border-radius: self.width / 2;
                            background: #FFFFFF;
                            Image {
                                source: @image-url("icons/mark-badge.png");
                                width: parent.width * Theme.mark-in-plate;
                                height: self.width;
                            }
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
                // The key, and a button that copies it: the CLI and chat both want text, not a
                // picture. An empty box of the button's width on the other side keeps the key
                // itself centred.
                if !root.scanning : HorizontalLayout {
                    alignment: center;
                    spacing: Theme.gap;
                    Rectangle { width: Theme.copy-target; }
                    VerticalLayout {
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
                    VerticalLayout {
                        alignment: center;
                        Rectangle {
                            width: Theme.copy-target;
                            height: self.width;
                            border-radius: self.width / 2;
                            background: copy-touch.pressed ? Theme.surface : transparent;
                            accessible-role: button;
                            accessible-label: root.copied ? "Key copied" : "Copy key";
                            accessible-action-default => { root.copy-key(); }
                            copy-touch := TouchArea {
                                mouse-cursor: pointer;
                                clicked => { root.copy-key(); }
                            }
                            Image {
                                source: root.copied ? @image-url("icons/check.svg") : @image-url("icons/copy.svg");
                                width: Theme.key-icon;
                                height: self.width;
                                colorize: root.copied ? Theme.answer : Theme.beacon;
                            }
                        }
                    }
                }

                // Naming a scanned key happens in a sheet over whatever screen you are on, so
                // this stays the two things that ever add someone.
                Wide {
                    text: root.scanning ? "Stop scanning" : "Scan a code";
                    primary: !root.scanning;
                    clicked => { root.scan(!root.scanning); }
                }
                if !root.scanning : Wide {
                    text: "Choose an image";
                    clicked => { root.pick(); }
                }
                if !root.scanning : Wide {
                    text: "Share my identity";
                    clicked => { root.share(); }
                }
            }
            // Whatever is left over ends up here rather than between the rows above.
            Rectangle { vertical-stretch: 1; }
        }
    }

    // A group of settings on its own inset surface. The surface does the grouping a label used
    // to, so no group is named.
    component SettingsGroup inherits Rectangle {
        background: Theme.surface;
        border-radius: Theme.radius;
        clip: true;
        VerticalLayout {
            @children
        }
    }

    // Between two rows of a group. It starts where the text does, so the tiles read as one
    // column rather than as boxes cut apart.
    component SettingsDivider inherits Rectangle {
        height: 1px;
        Rectangle {
            x: Theme.setting-pad + Theme.setting-tile + Theme.setting-gap;
            width: parent.width - self.x;
            height: parent.height;
            background: Theme.group-hairline;
        }
    }

    // What a setting is on the start side, what it is set to on the end side, and a tile with
    // its icon ahead of both. The value is in the accent only when tapping it does something.
    component SettingsRow inherits Rectangle {
        in property <image> icon;
        in property <string> title;
        in property <string> detail;
        in property <string> value;
        in property <bool> tappable: true;
        callback clicked();
        height: Theme.setting-height;
        // Must be constant; a row that does nothing when tapped says so through `enabled` below.
        accessible-role: button;
        accessible-enabled: root.tappable;
        accessible-label: root.title;
        accessible-description: root.detail;
        accessible-value: root.value;
        accessible-action-default => {
            if (root.tappable) {
                root.clicked();
            }
        }
        touch := TouchArea {
            enabled: root.tappable;
            mouse-cursor: root.tappable ? pointer : default;
            clicked => { root.clicked(); }
        }
        background: touch.pressed ? Theme.raised.with-alpha(0.6) : transparent;
        animate background { duration: 160ms; easing: ease-out; }
        HorizontalLayout {
            padding-left: Theme.setting-pad;
            padding-right: Theme.setting-pad;
            spacing: Theme.setting-gap;
            VerticalLayout {
                alignment: center;
                Rectangle {
                    width: Theme.setting-tile;
                    height: self.width;
                    border-radius: Theme.setting-tile-radius;
                    background: Theme.raised;
                    Image {
                        source: root.icon;
                        width: Theme.setting-icon;
                        height: self.width;
                        colorize: Theme.beacon;
                    }
                }
            }
            VerticalLayout {
                alignment: center;
                horizontal-stretch: 1;
                Text {
                    text: root.title;
                    color: Theme.text;
                    font-size: 1rem;
                    font-weight: 700;
                    height: Theme.setting-title-box;
                    vertical-alignment: center;
                    overflow: elide;
                }
                Text {
                    text: root.detail;
                    color: Theme.muted;
                    font-size: 0.8125rem;
                    height: Theme.setting-detail-box;
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

    component SettingsPage inherits Rectangle {
        in property <length> top-inset;
        in property <string> quality;
        in property <int> relays-on;
        in property <bool> battery-unrestricted;
        in property <bool> full-screen-calls;
        in property <bool> maker-list;
        callback share-diagnostics();
        callback edit-relays();
        callback allow-battery();
        callback allow-full-screen-calls();
        callback open-maker-list();
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
                    text: "Settings";
                    horizontal-stretch: 1;
                    vertical-alignment: center;
                }
                Brand { }
            }
            ScrollView {
                vertical-stretch: 1;
                // A finger drags the list itself; without this only the scrollbar moves it.
                mouse-drag-pan-enabled: true;
                VerticalLayout {
                    alignment: start;
                    padding-left: Theme.group-edge;
                    padding-right: Theme.group-edge;
                    padding-bottom: Theme.group-edge;
                    spacing: Theme.group-gap;
                    // First, because it is the one group that silently costs calls. The first
                    // two are read back from Android and say so; the maker's list cannot be read,
                    // so its row offers the screen and claims nothing.
                    SettingsGroup {
                        SettingsRow {
                            icon: @image-url("icons/battery.svg");
                            title: "Battery";
                            detail: "Wait for calls while asleep";
                            value: root.battery-unrestricted ? "Allowed" : "Allow";
                            tappable: !root.battery-unrestricted;
                            clicked => { root.allow-battery(); }
                        }
                        SettingsDivider { }
                        SettingsRow {
                            icon: @image-url("icons/lock.svg");
                            title: "Lock screen calls";
                            detail: "Ring over the lock screen";
                            value: root.full-screen-calls ? "Allowed" : "Allow";
                            tappable: !root.full-screen-calls;
                            clicked => { root.allow-full-screen-calls(); }
                        }
                        if root.maker-list : SettingsDivider { }
                        if root.maker-list : SettingsRow {
                            icon: @image-url("icons/apps.svg");
                            title: "Background apps";
                            detail: "Your phone's own list";
                            value: "Open";
                            clicked => { root.open-maker-list(); }
                        }
                    }
                    SettingsGroup {
                        SettingsRow {
                            icon: @image-url("icons/theme.svg");
                            title: "Theme";
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
                        SettingsDivider { }
                        SettingsRow {
                            icon: @image-url("icons/direction.svg");
                            title: "Layout direction";
                            detail: "Mirrors every screen";
                            value: Theme.rtl ? "RTL" : "LTR";
                            clicked => { Theme.rtl = !Theme.rtl; }
                        }
                    }
                    SettingsGroup {
                        SettingsRow {
                            icon: @image-url("icons/video.svg");
                            title: "Call quality";
                            detail: "Caps what the camera sends";
                            value: root.quality;
                            tappable: false;
                        }
                        SettingsDivider { }
                        // The count rather than the list: which relays are on is a detail, and
                        // how many is the part worth seeing without opening anything.
                        SettingsRow {
                            icon: @image-url("icons/relays.svg");
                            title: "Relays";
                            detail: "When a direct path won't form";
                            value: root.relays-on + " on";
                            clicked => { root.edit-relays(); }
                        }
                    }
                    // Not shown, sent. A log on a phone screen helps nobody; a log in a chat
                    // message is the only way a fault on someone else's phone reaches us.
                    SettingsGroup {
                        SettingsRow {
                            icon: @image-url("icons/share.svg");
                            title: "Diagnostics";
                            detail: "Send the log to the fixer";
                            value: "Share";
                            clicked => { root.share-diagnostics(); }
                        }
                    }
                }
            }
        }
    }

    // A call folded into a corner, so the rest of the app is usable while it runs. Adding a
    // contact or sending diagnostics mid-call should not mean hanging up, and picture-in-picture
    // does not help here — the user has not left uplink, which is the common case.
    //
    // It keeps its own position so a drag survives being restored and folded away again, and it
    // is clamped on every layout rather than only while dragging, so rotating cannot strand it
    // off screen.
    component MiniCall inherits Rectangle {
        in property <image> frame;
        in property <string> timer;
        // What it has to stay clear of at each end: the system bars, and the tab bar it would
        // otherwise sit on top of and make unreachable.
        in property <length> top-clear;
        in property <length> bottom-clear;
        // The window, which a component cannot ask for: inside it, `parent` is itself.
        in property <length> area-width;
        in property <length> area-height;
        in-out property <length> pos-x;
        in-out property <length> pos-y;
        in-out property <bool> placed;
        callback restore();

        width: Theme.mini-width;
        height: Theme.mini-height;
        // Clamped on every layout, not only while dragging, so a rotation cannot strand it off
        // screen.
        property <length> lowest: root.area-height - self.height - root.bottom-clear - Theme.mini-edge;
        x: clamp(root.pos-x, Theme.mini-edge, root.area-width - self.width - Theme.mini-edge);
        y: clamp(root.pos-y, root.top-clear + Theme.mini-edge, max(root.top-clear + Theme.mini-edge, lowest));
        // First shown, it rests in the corner nearest where the call controls were.
        init => {
            if (!root.placed) {
                root.pos-x = root.area-width - root.width - Theme.mini-edge;
                root.pos-y = root.lowest;
                root.placed = true;
            }
        }
        border-radius: Theme.radius;
        border-width: 1px;
        border-color: #FFFFFF3D;
        background: Theme.video;
        clip: true;
        drop-shadow-blur: 18px;
        drop-shadow-color: #00000080;

        Image {
            width: parent.width;
            height: parent.height;
            source: root.frame;
            image-fit: cover;
        }
        // The timer says it is still running even when the face has not arrived yet.
        Rectangle {
            y: parent.height - self.height;
            height: 22px;
            background: @linear-gradient(0deg, #04080DCC 0%, #04080D00 100%);
            Text {
                text: root.timer;
                color: Theme.on-video;
                font-family: Theme.mono;
                font-size: 0.6875rem;
                vertical-alignment: center;
            }
        }
        // `clicked` fires on release whether or not the finger travelled, so letting go of a drag
        // would also unfold the call. A tap is a press that stayed put.
        property <bool> dragged;
        TouchArea {
            pointer-event(event) => {
                if (event.kind == PointerEventKind.down) {
                    root.dragged = false;
                }
            }
            moved => {
                if (abs(self.mouse-x - self.pressed-x) > Theme.drag-slop
                    || abs(self.mouse-y - self.pressed-y) > Theme.drag-slop) {
                    root.dragged = true;
                }
                if (root.dragged) {
                    root.pos-x = root.x + self.mouse-x - self.pressed-x;
                    root.pos-y = root.y + self.mouse-y - self.pressed-y;
                }
            }
            clicked => {
                if (!root.dragged) {
                    root.restore();
                }
            }
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
        // "Direct" or "Relayed", empty until a path is known.
        in property <string> route;
        in property <string> status;
        // What the call is not managing to do, if anything: video that never started, say.
        in property <string> trouble;
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
        callback minimise();
        background: Theme.video;

        property <bool> connected: state == CallState.connected;

        Image {
            width: parent.width;
            height: parent.height;
            source: root.connected && !root.swapped ? root.remote-frame : root.frame;
            image-fit: cover;
        }

        // The corner card: the whole frame until the call connects, then a card holding whichever
        // of the two pictures is not filling the screen. It depends on `connected` alone — while
        // it also depended on `swapped`, a tap grew the card back to full size and buried the
        // other picture under it, which read as the self view vanishing rather than trading.
        self-view := Rectangle {
            property <bool> inset: root.connected;
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
                        // A call that is carrying less than it should says so. Audio with no
                        // video otherwise looks exactly like a call where nobody moved.
                        if root.trouble != "" : Text {
                            text: root.trouble;
                            color: Theme.end;
                            font-size: 0.8125rem;
                            height: self.font-size * Theme.line-box;
                            vertical-alignment: center;
                            overflow: elide;
                        }
                    }
                    // How the call is set up, in the order it matters: the route first, because
                    // a relayed call explains a bad one, then what it is encrypted with.
                    if root.connected && root.route != "" : VerticalLayout {
                        alignment: start;
                        Rectangle {
                            height: Theme.chip-height;
                            border-radius: self.height / 2;
                            background: #E9F0F71A;
                            border-width: 1px;
                            border-color: #E9F0F73D;
                            HorizontalLayout {
                                padding-left: Theme.chip-pad;
                                padding-right: Theme.chip-pad;
                                Text {
                                    text: root.route;
                                    color: Theme.on-video-muted;
                                    font-family: Theme.mono;
                                    font-size: 0.75rem;
                                    letter-spacing: 0.34px;
                                    vertical-alignment: center;
                                }
                            }
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
                    // Folds the call away rather than ending it, which is the difference between
                    // looking something up mid-call and having to call back.
                    if root.connected : Key {
                        icon: @image-url("icons/minimise.svg");
                        label: "Fold away";
                        clicked => { root.minimise(); }
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
        // The share of the code's width the mark in its middle may cover; uplink-core picks it.
        in property <float> qr-mark;
        in property <string> my-id;
        in property <[string]> my-fingerprint-lines;
        in property <string> my-short-fingerprint;
        in property <string> call-status;
        in property <string> call-route;
        // A running call folded into a corner so the rest of the app can be used. Its position
        // is kept here so a drag outlives being restored and folded away again.
        in-out property <bool> call-folded: false;
        // The window itself has been shrunk by the system. Everything but the picture goes: at
        // that size chrome is unreadable, and the controls are the window's own buttons.
        in property <bool> call-pip: false;
        in-out property <length> call-fold-x;
        in-out property <length> call-fold-y;
        in-out property <bool> call-fold-placed: false;
        // Cleared when a call starts; set if part of it never came up.
        in-out property <string> call-trouble;
        in property <string> peer-name;
        in property <string> peer-initial: "?";
        in property <string> call-timer;
        in property <string> key-exchange;
        in property <[ContactItem]> contacts;
        in property <[CallItem]> calls;
        in property <bool> online: false;
        in-out property <string> toast;
        in-out property <Confirm> confirming: Confirm.none;
        in-out property <bool> selecting: false;
        in property <int> selected-count: 0;
        // The contact being looked at. Empty means none, which is also how it is dismissed.
        in-out property <string> open-contact-id;
        in property <string> open-contact-name;
        in property <string> open-contact-advertised;
        in property <string> open-contact-initial;
        in property <bool> open-contact-favourite;
        in property <[string]> open-contact-fingerprint;
        in property <string> quality: "720p · 30";

        in property <bool> booting: true;
        in property <bool> gate: false;
        // Nothing explains itself before it has been asked, so a fresh install would otherwise
        // look identical to a refusal.
        in-out property <bool> asked: false;
        in property <bool> permissions-blocked: false;
        in property <[PermissionItem]> permissions;
        in-out property <Screen> screen: Screen.people;
        in-out property <bool> scanning: false;
        in-out property <bool> swapped: false;
        in-out property <bool> mic-on: true;
        in-out property <bool> speaker-on: true;
        in-out property <string> peer-key;
        in-out property <string> new-name;
        in-out property <string> add-error;
        // Where in the People list a just-added contact sits, so the list opens on it; negative
        // when there is nothing to show.
        in property <length> contacts-reveal: -1px;
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
        callback copy-key();
        // A key read off the camera, handed over from the decoding thread to be offered or
        // said to be known already. Rust's own; no markup invokes it.
        callback key-found(string);
        callback share-diagnostics();
        // Which relays the endpoint may use. The sheet edits this model in place; saving writes
        // it and rebinds, cancelling throws it away and reads the stored set back.
        in property <[RelayItem]> relays;
        in property <[RelayItem]> custom-relays;
        // How many are on, counted where the model is built rather than in the markup.
        in property <int> relays-on;
        in-out property <bool> relays-custom;
        // Read from Android on start and on every return to the front, since each of them is
        // changed in a system screen the app sends the user to. Assumed fine until read.
        in property <bool> battery-unrestricted: true;
        in property <bool> full-screen-calls: true;
        in property <bool> maker-list: false;
        callback allow-battery();
        callback allow-full-screen-calls();
        callback open-maker-list();
        // The one-time explainer for the battery exemption, shown after the gate.
        in-out property <bool> battery-ask: false;
        callback skip-battery();
        in-out property <bool> editing-relays: false;
        in-out property <string> new-relay-name;
        in-out property <string> new-relay-url;
        callback save-relays();
        callback cancel-relays();
        callback add-relay(string, string);
        callback remove-relay(string);
        callback scan(bool);
        callback grant-permissions();
        callback open-settings();
        callback clear-calls();
        callback open-contact(string);
        callback set-favourite(string, bool);
        callback toggle-selected(string);
        callback long-pressed();
        callback clear-selection();
        callback remove-selected();
        // The system back gesture, already accepted by `back-stop`. Whoever handles it closes the
        // innermost thing on screen, or steps the app into the background.
        callback back();
        // The window draws under the system bars, so what shows through them is our own ground.
        // Android paints their icons, and only we know which shade they have to read against.
        // A change callback is evaluated eagerly, so this fires even though nothing reads it.
        // What is behind the bars is whatever is on top: a call, a dimmed sheet and the log are
        // dark whatever the theme says, so only a bare page follows it.
        callback bars-changed(bool);
        // The two settings the user can change. Read back rather than driven from here: the rows
        // write straight to the theme, and this says so afterwards for whoever stores it.
        callback appearance-changed(Appearance);
        out property <Appearance> appearance: Theme.appearance;
        changed appearance => { root.appearance-changed(self.appearance); }
        callback rtl-changed(bool);
        out property <bool> rtl: Theme.rtl;
        changed rtl => { root.rtl-changed(self.rtl); }
        out property <bool> bars-light: !Theme.dark
            && root.call-state == CallState.idle
            && root.confirming == Confirm.none
            && !root.editing-relays
            && root.peer-key == "";
        changed bars-light => { root.bars-changed(self.bars-light); }

        title: "uplink";
        background: Theme.ground;
        // 1rem is Material 3's body-large, so the design's sizes read as their roles: 1.5rem
        // headline-small, 0.8125rem the 13px fingerprints. Android scales it with the system size.
        default-font-size: 16px;
        // Prose is Public Sans; Outfit and Plex Mono are asked for by the elements that want them.
        default-font-family: "Public Sans";
        // The palette is left alone: Slint's Android backend fills it from the system's night
        // mode, and writing it here would pin the app to one theme and make `Theme.system-dark`
        // read back whatever we just wrote.

        // A call that ends while folded unfolds, so the next one opens on the call screen. These
        // sit outside the scope below because `changed` belongs to whatever declares it.
        changed call-state => {
            if (self.call-state == CallState.idle) {
                self.call-folded = false;
            }
        }
        // A second message restarts the clock instead of inheriting what is left of the first's.
        changed toast => {
            if (self.toast != "") {
                toast-timer.restart();
            }
        }

        // Everything lives inside this, so that Back reaches it. Slint's Android backend turns
        // the system gesture into a `Key.Back` press, and if nothing *accepts* it, finishes the
        // activity — which for this app means dropping the endpoint that incoming calls arrive
        // at. Rejected keys travel up to the parent, so a scope wrapping the window sees Back
        // even when a text field has focus and ignored it. Accepting it always is the point.
        back-stop := FocusScope {
            // Qualified: a bare `accept` would resolve to this window's own `accept` callback,
            // the one that answers a ringing call.
            key-pressed(event) => {
                if (event.text == Key.Back) {
                    root.back();
                    return EventResult.accept;
                }
                return EventResult.reject;
            }
            // Nothing else takes focus on its own, so keys start here; anything that borrows it
            // rejects Back anyway, and a rejected key travels back up to this scope.
            init => { self.focus(); }

            // Pages and the tab bar share the window: the bar takes space, not the content's.
            VerticalLayout {
                Rectangle {
                    vertical-stretch: 1;
                    clip: true;
                    if root.screen == Screen.people : PeoplePage {
                        top-inset: root.safe-area-insets.top;
                        online: root.online;
                        contacts: root.contacts;
                        reveal: root.contacts-reveal;
                        selecting: root.selecting;
                        selected-count: root.selected-count;
                        call(id) => { root.call(id); }
                        open(id) => { root.open-contact(id); }
                        connect => { root.screen = Screen.connect; }
                        select(id) => { root.toggle-selected(id); }
                        start-selecting => { root.selecting = true; }
                        long-pressed => { root.long-pressed(); }
                        remove-selected => { root.confirming = Confirm.remove-selected; }
                    }
                    if root.screen == Screen.calls : CallsPage {
                        top-inset: root.safe-area-insets.top;
                        calls: root.calls;
                        call(id) => { root.call(id); }
                        clear => { root.confirming = Confirm.clear-calls; }
                    }
                    if root.screen == Screen.connect : KeyPage {
                        top-inset: root.safe-area-insets.top;
                        qr: root.qr;
                        mark: root.qr-mark;
                        frame: root.frame;
                        fingerprint-lines: root.my-fingerprint-lines;
                        scanning: root.scanning;
                        pending-key: root.peer-key;
                        new-name <=> root.new-name;
                        scan(on) => { root.scan(on); }
                        pick => { root.pick-key(); }
                        share => { root.share-key(); }
                        copy => { root.copy-key(); }
                        add(name, key) => { root.add-contact(name, key); }
                    }
                    if root.screen == Screen.settings : SettingsPage {
                        top-inset: root.safe-area-insets.top;
                        quality: root.quality;
                        relays-on: root.relays-on;
                        battery-unrestricted: root.battery-unrestricted;
                        full-screen-calls: root.full-screen-calls;
                        maker-list: root.maker-list;
                        share-diagnostics => { root.share-diagnostics(); }
                        edit-relays => { root.editing-relays = true; }
                        allow-battery => { root.allow-battery(); }
                        allow-full-screen-calls => { root.allow-full-screen-calls(); }
                        open-maker-list => { root.open-maker-list(); }
                    }
                }
                Tabs {
                    screen <=> root.screen;
                    bottom-inset: root.safe-area-insets.bottom;
                }
            }

            // Shrunk by the system: the remote picture fills the window and nothing else is
            // drawn, because the system draws the buttons over it and there is room for no more.
            if root.call-pip : Rectangle {
                background: Theme.video;
                Image {
                    width: parent.width;
                    height: parent.height;
                    // Always the other person, whatever the full screen was showing when it
                    // shrank. A window this small is for watching them, not yourself.
                    source: root.remote-frame;
                    image-fit: cover;
                }
            }

            // Overlays: declared last, so they cover the pages and the tab bar.
            if root.call-state != CallState.idle && !root.call-folded && !root.call-pip : CallScreen {
                frame: root.frame;
                remote-frame: root.remote-frame;
                peer-name: root.peer-name;
                peer-initial: root.peer-initial;
                timer: root.call-timer;
                key-exchange: root.key-exchange;
                route: root.call-route;
                status: root.call-status;
                trouble: root.call-trouble;
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
                minimise => { root.call-folded = true; }
            }

            // The folded call, over the pages and the tab bar but under anything that wants an
            // answer.
            if root.call-state != CallState.idle && root.call-folded && !root.call-pip : MiniCall {
                // The other person, like the shrunken window: your own face is the one thing you
                // do not need a corner of the screen for.
                frame: root.remote-frame;
                timer: root.call-timer;
                top-clear: root.safe-area-insets.top;
                bottom-clear: root.safe-area-insets.bottom + Theme.tab-height;
                area-width: root.width;
                area-height: root.height;
                pos-x <=> root.call-fold-x;
                pos-y <=> root.call-fold-y;
                placed <=> root.call-fold-placed;
                restore => { root.call-folded = false; }
            }

            // Something that went wrong and needs no decision — it says its piece and goes.
            // Anything the user has to answer is a sheet, not this. Slint has no toast widget,
            // but it has a Timer, so the dismissal lives here rather than in every caller.
            toast-timer := Timer {
                interval: Theme.toast-life;
                running: root.toast != "";
                triggered() => { root.toast = ""; }
            }
            if root.toast != "" : Rectangle {
                y: parent.height - self.height - root.safe-area-insets.bottom - Theme.toast-bottom;
                height: toast-body.preferred-height;
                width: min(parent.width - Theme.edge * 2, Theme.sheet-width);
                border-radius: Theme.wide-radius;
                background: Theme.raised;
                border-width: 1px;
                border-color: Theme.hairline;
                toast-body := HorizontalLayout {
                    padding: Theme.row-gap;
                    Text {
                        text: root.toast;
                        color: Theme.text;
                        font-size: 0.875rem;
                        horizontal-alignment: center;
                        wrap: word-wrap;
                    }
                }
            }

            if root.editing-relays : RelaySheet {
                relays: root.relays;
                custom-relays: root.custom-relays;
                custom <=> root.relays-custom;
                new-name <=> root.new-relay-name;
                new-url <=> root.new-relay-url;
                keyboard: root.virtual-keyboard-size.height;
                add(name, url) => { root.add-relay(name, url); }
                remove(host) => { root.remove-relay(host); }
                save => {
                    root.editing-relays = false;
                    root.save-relays();
                }
                cancel => {
                    root.editing-relays = false;
                    root.cancel-relays();
                }
            }

            // A key that has arrived and has no name yet, over everything below it.
            if root.peer-key != "" : NameSheet {
                key: root.peer-key;
                keyboard: root.virtual-keyboard-size.height;
                name <=> root.new-name;
                error <=> root.add-error;
                add(name) => { root.add-contact(name, root.peer-key); }
                cancel => {
                    root.peer-key = "";
                    root.new-name = "";
                    root.add-error = "";
                }
            }

            // A contact takes the window, over the tabs, until it is dismissed.
            if root.open-contact-id != "" : ContactPage {
                top-inset: root.safe-area-insets.top;
                bottom-inset: root.safe-area-insets.bottom;
                name: root.open-contact-name;
                advertised: root.open-contact-advertised;
                initial: root.open-contact-initial;
                favourite: root.open-contact-favourite;
                fingerprint-lines: root.open-contact-fingerprint;
                draft <=> root.new-name;
                call => { root.call(root.open-contact-id); }
                toggle-favourite => { root.set-favourite(root.open-contact-id, !root.open-contact-favourite); }
                rename(name) => { root.rename-contact(root.open-contact-id, name); }
                remove => { root.confirming = Confirm.remove-contact; }
                close => { root.open-contact-id = ""; }
            }

            // Nothing here happens on the tap that asked for it. Above every page and sheet that
            // can ask, since later siblings draw on top: under the contact page it was invisible,
            // and closing the page to find it cleared the contact it was meant to remove.
            if root.confirming != Confirm.none : ConfirmSheet {
                title: root.confirming == Confirm.clear-calls ? "Clear the call log?"
                    : root.confirming == Confirm.remove-selected ? "Remove " + root.selected-count + " contacts?"
                    : "Remove " + root.open-contact-name + "?";
                body: root.confirming == Confirm.clear-calls
                    ? "Every call is forgotten. It does not affect your contacts."
                    : "Their key goes with them. You would need it again to call them, and they can still call you.";
                confirm-label: root.confirming == Confirm.clear-calls ? "Clear" : "Remove";
                confirm => {
                    if (root.confirming == Confirm.clear-calls) {
                        root.clear-calls();
                    } else if (root.confirming == Confirm.remove-selected) {
                        root.remove-selected();
                    } else {
                        root.remove-contact(root.open-contact-id);
                        root.open-contact-id = "";
                    }
                    root.confirming = Confirm.none;
                }
                cancel => { root.confirming = Confirm.none; }
            }

            // Nothing behind this is reachable until all three are granted.
            if root.gate : PermissionsPage {
                top-inset: root.safe-area-insets.top;
                permissions: root.permissions;
                blocked: root.permissions-blocked;
                grant => { root.grant-permissions(); }
                settings => { root.open-settings(); }
            }

            if root.battery-ask && !root.gate : BatteryPage {
                top-inset: root.safe-area-insets.top;
                allow => { root.allow-battery(); }
                skip => { root.skip-battery(); }
            }

            // Over everything until the endpoint is up. It fades rather than being torn out: the
            // ground does not change across the handover, so all that leaves is the mark.
            splash := Splash {
                opacity: root.booting ? 1.0 : 0.0;
                visible: self.opacity > 0.0;
                animate opacity { duration: 420ms; easing: ease-out; }
            }
        }
    }
}

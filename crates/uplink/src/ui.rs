//! The Slint UI: tokens from docs/plan.md, a call screen that owns the frame, and the
//! people/identity/settings screens behind it.
//!
//! Structure: one root `VerticalLayout` holds the page area and the tab bar, so the bar takes
//! space instead of covering content; the in-call screen and the log are declared after it, as
//! overlays (later siblings draw on top). Layout is written start/end (mirrored by `Theme.rtl`),
//! never left/right, because Slint does not mirror layouts itself.

slint::slint! {
    import { Button, LineEdit, ScrollView, VerticalBox, HorizontalBox, Palette } from "std-widgets.slint";

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

    export global Theme {
        // Dark-first: a call is video, and video wants a dark room.
        out property <color> ink: #070A0F;
        out property <color> surface: #101720;
        out property <color> raised: #18212C;
        out property <color> line: #263140;
        out property <color> text: #E9F0F7;
        out property <color> muted: #93A3B5;
        out property <color> beacon: #58B6FF;
        out property <color> answer: #32C97A;
        out property <color> end: #FF5252;
        // One spacing scale: 8, halved to 4 where tight.
        out property <length> gap: 8px;
        out property <length> gap-large: 16px;
        out property <length> radius: 14px;
        out property <[color]> avatar-tints: [#D8E7F5, #F6DDE6, #DCEEDF, #F3E3CF, #E0DDF5];
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
            color: #123A5C;
            font-size: size / 2.4;
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
        width: danger || positive ? 72px : 56px;
        height: 56px;
        border-radius: 28px;
        background: danger ? Theme.end
            : positive ? Theme.answer
            : on ? Theme.text
            : #E9F0F71F;
        border-width: danger || positive || on ? 0px : 1px;
        border-color: #E9F0F72B;
        accessible-role: button;
        accessible-label: label;
        accessible-action-default => { root.clicked(); }
        Image {
            source: root.icon;
            width: 24px;
            height: 24px;
            colorize: danger || positive ? #FFFFFF : on ? Theme.ink : Theme.text;
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
        font-size: 1.6rem;
        font-weight: 700;
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

    // A pill for a screen's one header action.
    component Action inherits Rectangle {
        in property <string> text;
        callback clicked();
        height: 34px;
        width: label.preferred-width + 28px;
        border-radius: 17px;
        background: Theme.beacon.with-alpha(0.14);
        border-width: 1px;
        border-color: Theme.beacon.with-alpha(0.35);
        accessible-role: button;
        accessible-label: text;
        accessible-action-default => { root.clicked(); }
        label := Text {
            text: root.text;
            color: Theme.beacon;
            font-size: 0.9rem;
        }
        touch := TouchArea {
            mouse-cursor: pointer;
            clicked => { root.clicked(); }
        }
        states [ pressed when touch.pressed : { opacity: 0.6; } ]
        animate opacity { duration: 160ms; easing: ease-out; }
    }

    component PeoplePage inherits Rectangle {
        in property <[ContactItem]> contacts;
        in property <length> top-inset;
        in property <string> status;
        callback call(string);
        callback remove(string);
        callback add-someone();
        background: Theme.ink;

        VerticalBox {
            padding-top: root.top-inset + Theme.gap;
            spacing: Theme.gap;
            HorizontalLayout {
                vertical-stretch: 0;
                spacing: Theme.gap;
                PageTitle {
                    text: "People";
                    horizontal-stretch: 1;
                    vertical-alignment: center;
                }
                Action {
                    text: "+ Add";
                    clicked => { root.add-someone(); }
                }
            }
            if root.status != "" : Text {
                text: root.status;
                color: Theme.beacon;
                font-size: 0.85rem;
                wrap: word-wrap;
            }

            if root.contacts.length == 0 : Rectangle {
                vertical-stretch: 1;
                VerticalLayout {
                    alignment: center;
                    spacing: Theme.gap;
                    Text {
                        text: "No one here yet";
                        color: Theme.text;
                        font-size: 1.1rem;
                        horizontal-alignment: center;
                    }
                    Text {
                        text: "Add someone from their code, and they appear here to call.";
                        color: Theme.muted;
                        font-size: 0.9rem;
                        horizontal-alignment: center;
                        wrap: word-wrap;
                    }
                }
            }
            if root.contacts.length > 0 : ScrollView {
                vertical-stretch: 1;
                VerticalLayout {
                    alignment: start;
                    spacing: Theme.gap;
                    padding-bottom: Theme.gap;
                    // Calling is the row's job, so the whole row is the target; removing is rare
                    // and quiet at the end.
                    for contact[index] in root.contacts : Card {
                        height: 68px;
                        accessible-role: button;
                        accessible-label: "Call " + contact.name;
                        accessible-action-default => { root.call(contact.id); }
                        row-touch := TouchArea {
                            mouse-cursor: pointer;
                            clicked => { root.call(contact.id); }
                        }
                        background: row-touch.pressed ? Theme.raised : Theme.surface;
                        animate background { duration: 160ms; easing: ease-out; }
                        HorizontalBox {
                            spacing: Theme.gap;
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
                                    color: Theme.text;
                                    font-size: 1rem;
                                    font-weight: 600;
                                    overflow: elide;
                                }
                                Text {
                                    text: contact.fingerprint;
                                    color: Theme.muted;
                                    font-size: 0.7rem;
                                    font-family: "monospace";
                                    overflow: elide;
                                }
                            }
                            Rectangle {
                                width: 32px;
                                accessible-role: button;
                                accessible-label: "Remove " + contact.name;
                                accessible-action-default => { root.remove(contact.id); }
                                Image {
                                    source: @image-url("../icons/close.svg");
                                    width: 16px;
                                    height: 16px;
                                    colorize: Theme.muted;
                                }
                                TouchArea {
                                    mouse-cursor: pointer;
                                    clicked => { root.remove(contact.id); }
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
        in property <image> frame;
        in property <string> fingerprint;
        in property <bool> scanning;
        in property <string> pending-key;
        in-out property <string> new-name;
        callback scan(bool);
        callback pick();
        callback share();
        callback add(string, string);
        background: Theme.ink;

        VerticalBox {
            padding-top: root.top-inset + Theme.gap;
            spacing: Theme.gap;
            PageTitle { text: root.scanning ? "Point at their code" : "Your key"; }

            // Code, fingerprint and actions are one object, so they stay together; the slack
            // goes to a plain Rectangle below them. The row centres the card — a fixed-width
            // child of a vertical layout would otherwise sit against the start edge.
            HorizontalLayout {
                alignment: center;
                vertical-stretch: 0;
                Rectangle {
                    width: min(root.width - Theme.gap-large * 2, 300px);
                    height: self.width;
                    border-radius: Theme.radius;
                    background: root.scanning ? Theme.surface : #FFFFFF;
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
                text: root.scanning ? "Hold their code steady in the frame" : root.fingerprint;
                color: root.scanning ? Theme.muted : Theme.text;
                font-size: root.scanning ? 0.9rem : 0.95rem;
                font-family: root.scanning ? "" : "monospace";
                horizontal-alignment: center;
                wrap: word-wrap;
            }

            // Once a key is in hand (scanned, read from an image, or pasted) this is all that is
            // left to do: give them a name.
            if root.pending-key != "" && !root.scanning : Card {
                height: naming.preferred-height;
                naming := VerticalBox {
                    spacing: Theme.gap;
                    Text {
                        text: "Add this key";
                        color: Theme.text;
                        font-size: 0.95rem;
                        font-weight: 600;
                    }
                    Text {
                        text: root.pending-key;
                        color: Theme.muted;
                        font-size: 0.7rem;
                        font-family: "monospace";
                        overflow: elide;
                    }
                    LineEdit {
                        placeholder-text: "Their name";
                        text <=> root.new-name;
                    }
                    Button {
                        text: "Add";
                        primary: true;
                        enabled: root.new-name != "";
                        clicked => { root.add(root.new-name, root.pending-key); }
                    }
                }
            }

            VerticalLayout {
                vertical-stretch: 0;
                spacing: Theme.gap;
                Button {
                    text: root.scanning ? "Stop scanning" : "Scan a code";
                    primary: !root.scanning;
                    clicked => { root.scan(!root.scanning); }
                }
                if !root.scanning : Button {
                    text: "Read a key from an image";
                    clicked => { root.pick(); }
                }
                if !root.scanning : Button {
                    text: "Share my key";
                    clicked => { root.share(); }
                }
            }
            // Whatever is left over ends up here rather than between the rows above.
            Rectangle { vertical-stretch: 1; }
        }
    }

    // A row in the settings list: what it is on the left, what it is set to on the right.
    component SettingsRow inherits Rectangle {
        in property <string> title;
        in property <string> detail;
        in property <string> value;
        in property <bool> tappable: true;
        in property <bool> first: false;
        callback clicked();
        height: 64px;
        accessible-role: button;
        accessible-label: title;
        accessible-action-default => { root.clicked(); }
        if !root.first : Rectangle {
            y: 0;
            height: 1px;
            background: Theme.line;
        }
        touch := TouchArea {
            mouse-cursor: root.tappable ? pointer : default;
            clicked => {
                if (root.tappable) {
                    root.clicked();
                }
            }
        }
        background: touch.pressed && root.tappable ? Theme.raised : transparent;
        animate background { duration: 160ms; easing: ease-out; }
        HorizontalBox {
            padding-left: Theme.gap-large;
            padding-right: Theme.gap-large;
            spacing: Theme.gap;
            VerticalLayout {
                alignment: center;
                spacing: 1px;
                horizontal-stretch: 1;
                Text {
                    text: root.title;
                    color: Theme.text;
                    font-size: 0.95rem;
                    overflow: elide;
                }
                Text {
                    text: root.detail;
                    color: Theme.muted;
                    font-size: 0.75rem;
                    overflow: elide;
                }
            }
            VerticalLayout {
                alignment: center;
                Text {
                    text: root.value;
                    color: root.tappable ? Theme.beacon : Theme.muted;
                    font-size: 0.85rem;
                    font-family: "monospace";
                }
            }
        }
    }

    component SettingsPage inherits Rectangle {
        in property <length> top-inset;
        in property <string> quality;
        callback open-log();
        background: Theme.ink;

        VerticalBox {
            padding-top: root.top-inset + Theme.gap;
            spacing: Theme.gap;
            PageTitle { text: "Settings"; }
            Card {
                height: rows.preferred-height;
                clip: true;
                rows := VerticalLayout {
                    SettingsRow {
                        first: true;
                        title: "Appearance";
                        detail: "A call wants a dark room";
                        value: "Dark";
                        tappable: false;
                    }
                    SettingsRow {
                        title: "Layout direction";
                        detail: "Mirrors every screen, for Arabic";
                        value: Theme.rtl ? "Right to left" : "Left to right";
                        clicked => { Theme.rtl = !Theme.rtl; }
                    }
                    SettingsRow {
                        title: "Call quality";
                        detail: "What your camera sends";
                        value: root.quality;
                        tappable: false;
                    }
                    SettingsRow {
                        title: "Diagnostics";
                        detail: "What this run has been doing";
                        value: "Open";
                        clicked => { root.open-log(); }
                    }
                }
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
        background: Theme.ink;

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
            x: inset ? (Theme.rtl ? Theme.gap-large : parent.width - self.width - Theme.gap-large) : 0px;
            y: inset ? root.top-inset + 72px : 0px;
            width: inset ? 108px : parent.width;
            height: inset ? 160px : parent.height;
            border-radius: inset ? Theme.radius : 0px;
            border-width: inset ? 1px : 0px;
            border-color: #FFFFFF2E;
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
                spacing: Theme.gap-large;
                HorizontalLayout {
                    alignment: center;
                    Avatar {
                        initial: root.peer-initial;
                        tint: 1;
                        size: 104px;
                    }
                }
                Text {
                    text: root.peer-name;
                    color: Theme.text;
                    font-size: 1.8rem;
                    font-weight: 600;
                    horizontal-alignment: center;
                }
                Text {
                    text: root.state == CallState.incoming ? "Incoming video call" : "Calling…";
                    color: Theme.muted;
                    font-size: 1rem;
                    horizontal-alignment: center;
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
                padding-top: root.top-inset + Theme.gap;
                padding-left: Theme.gap-large;
                padding-right: Theme.gap-large;
                alignment: start;
                spacing: 2px;
                HorizontalLayout {
                    alignment: space-between;
                    spacing: Theme.gap;
                    VerticalLayout {
                        spacing: 2px;
                        Text {
                            text: root.peer-name;
                            color: Theme.text;
                            font-size: 1.2rem;
                            font-weight: 600;
                        }
                        Text {
                            text: root.connected ? root.timer : root.status;
                            color: Theme.muted;
                            font-size: 0.85rem;
                            font-family: root.connected ? "monospace" : "";
                        }
                    }
                    if root.connected && root.key-exchange != "" : Rectangle {
                        height: 24px;
                        border-radius: 12px;
                        background: Theme.beacon.with-alpha(0.16);
                        border-width: 1px;
                        border-color: Theme.beacon.with-alpha(0.35);
                        HorizontalLayout {
                            padding-left: Theme.gap;
                            padding-right: Theme.gap;
                            Text {
                                text: root.key-exchange;
                                color: Theme.beacon;
                                font-size: 0.7rem;
                                vertical-alignment: center;
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
                padding-bottom: root.bottom-inset + Theme.gap-large;
                HorizontalLayout {
                    alignment: center;
                    spacing: Theme.gap-large;
                    if root.state == CallState.incoming : Key {
                        icon: @image-url("../icons/close.svg");
                        label: "Decline";
                        danger: true;
                        clicked => { root.reject(); }
                    }
                    if root.state == CallState.incoming : Key {
                        icon: @image-url("../icons/call.svg");
                        label: "Answer";
                        positive: true;
                        clicked => { root.accept(); }
                    }
                    if root.state != CallState.incoming : Key {
                        icon: root.mic-on ? @image-url("../icons/mic.svg") : @image-url("../icons/mic-off.svg");
                        label: root.mic-on ? "Mute" : "Unmute";
                        on: !root.mic-on;
                        clicked => { root.toggle-mic(); }
                    }
                    if root.connected : Key {
                        icon: @image-url("../icons/speaker.svg");
                        label: "Speaker";
                        on: root.speaker-on;
                        clicked => { root.toggle-speaker(); }
                    }
                    if root.connected : Key {
                        icon: @image-url("../icons/camera-switch.svg");
                        label: "Switch camera";
                        clicked => { root.flip-camera(); }
                    }
                    if root.state != CallState.incoming : Key {
                        icon: @image-url("../icons/call-end.svg");
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
        in property <string> my-fingerprint;
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
        background: Theme.ink;
        default-font-size: 15px;
        // Our surfaces are dark, so the std-widgets follow rather than fight them.
        init => { Palette.color-scheme = ColorScheme.dark; }

        // Pages and the tab bar share the window: the bar takes space, it does not cover content.
        VerticalLayout {
            Rectangle {
                vertical-stretch: 1;
                clip: true;
                if root.screen == Screen.people : PeoplePage {
                    top-inset: root.safe-area-insets.top;
                    status: root.call-status;
                    contacts: root.contacts;
                    call(id) => { root.call(id); }
                    remove(id) => { root.remove-contact(id); }
                    add-someone => { root.screen = Screen.identity; }
                }
                if root.screen == Screen.identity : KeyPage {
                    top-inset: root.safe-area-insets.top;
                    qr: root.qr;
                    frame: root.frame;
                    fingerprint: root.my-fingerprint;
                    scanning: root.scanning;
                    pending-key: root.peer-key;
                    new-name <=> root.new-name;
                    scan(on) => { root.scan(on); }
                    pick => { root.pick-key(); }
                    share => { root.share-key(); }
                    add(name, key) => { root.add-contact(name, key); }
                }
                if root.screen == Screen.settings : SettingsPage {
                    top-inset: root.safe-area-insets.top;
                    quality: root.quality;
                    open-log => { root.log-open = true; }
                }
            }
            Tabs {
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
                            font-family: "monospace";
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
                        font-family: "monospace";
                        wrap: word-wrap;
                    }
                }
            }
        }
    }
}

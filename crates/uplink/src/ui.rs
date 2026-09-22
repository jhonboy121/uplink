slint::slint! {
    import { Button, HorizontalBox, LineEdit, ScrollView, Spinner, VerticalBox } from "std-widgets.slint";

    export enum CallState { idle, dialing, ringing, incoming, connected }

    export component App inherits Window {
        in property <image> frame;
        in property <string> stats;
        in property <string> log;
        in-out property <bool> log-open;
        in property <string> my-id;
        in property <string> call-status;
        in-out property <string> peer-key;
        in property <CallState> call-state;
        callback start();
        callback stop();
        callback flip();
        callback rotate();
        callback mirror();
        callback call(string);
        callback accept();
        callback reject();
        callback hangup();
        background: #000;

        VerticalBox {
            padding-top: root.safe-area-insets.top + 4px;
            padding-bottom: max(root.safe-area-insets.bottom, root.virtual-keyboard-size.height) + 4px;
            HorizontalBox {
                padding: 0;
                Text { text: root.stats; color: #9f9; font-size: 12px; vertical-alignment: center; horizontal-stretch: 1; }
                Button { text: "Log"; clicked => { root.log-open = true; } }
            }
            Image {
                source: root.frame;
                image-fit: contain;
                vertical-stretch: 1;
            }
            HorizontalBox {
                Button { text: "Start"; clicked => { root.start(); } }
                Button { text: "Stop"; clicked => { root.stop(); } }
                Button { text: "Flip"; clicked => { root.flip(); } }
                Button { text: "Rot"; clicked => { root.rotate(); } }
                Button { text: "Mir"; clicked => { root.mirror(); } }
            }
            Text { text: "my key"; color: #aaa; font-size: 10px; }
            LineEdit { text: root.my-id; read-only: true; font-size: 11px; }
            LineEdit {
                placeholder-text: "peer key";
                text <=> root.peer-key;
                font-size: 11px;
                enabled: root.call-state == CallState.idle;
            }
            if root.call-state == CallState.idle: HorizontalBox {
                Button {
                    text: "Call";
                    enabled: root.peer-key != "";
                    clicked => { root.call(root.peer-key); }
                }
            }
            if root.call-state == CallState.dialing || root.call-state == CallState.ringing: HorizontalBox {
                Spinner { indeterminate: true; width: 24px; height: 24px; }
                Text {
                    text: root.call-state == CallState.dialing ? "Calling…" : "Ringing…";
                    color: #ddd;
                    vertical-alignment: center;
                }
                Button { text: "Cancel"; clicked => { root.hangup(); } }
            }
            if root.call-state == CallState.incoming: HorizontalBox {
                Text { text: "Incoming call"; color: #ddd; vertical-alignment: center; }
                Button { text: "Accept"; primary: true; clicked => { root.accept(); } }
                Button { text: "Reject"; clicked => { root.reject(); } }
            }
            if root.call-state == CallState.connected: HorizontalBox {
                Text { text: "Connected"; color: #9f9; vertical-alignment: center; }
                Button { text: "Hang up"; clicked => { root.hangup(); } }
            }
            Text { text: root.call-status; color: #8cf; font-size: 12px; wrap: word-wrap; }
        }

        if root.log-open: Rectangle {
            background: #000000f0;
            VerticalBox {
                padding-top: root.safe-area-insets.top + 4px;
                padding-bottom: root.safe-area-insets.bottom + 4px;
                HorizontalBox {
                    padding: 0;
                    Text { text: "Log"; color: #ddd; font-size: 16px; vertical-alignment: center; horizontal-stretch: 1; }
                    Button { text: "Close"; clicked => { root.log-open = false; } }
                }
                ScrollView {
                    vertical-stretch: 1;
                    Text { text: root.log; color: #bbb; font-size: 10px; wrap: word-wrap; }
                }
            }
        }
    }
}

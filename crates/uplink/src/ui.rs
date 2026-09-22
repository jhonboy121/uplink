slint::slint! {
    import { Button, HorizontalBox, VerticalBox } from "std-widgets.slint";

    export component App inherits Window {
        in property <image> frame;
        in property <string> stats;
        in property <string> status;
        callback start();
        callback stop();
        callback flip();
        callback rotate();
        callback mirror();
        background: #000;

        VerticalBox {
            padding-top: root.safe-area-insets.top + 4px;
            padding-bottom: root.safe-area-insets.bottom + 4px;
            Image {
                source: root.frame;
                image-fit: contain;
                vertical-stretch: 1;
            }
            Text { text: root.stats; color: #9f9; font-size: 12px; }
            Text { text: root.status; color: #fc6; font-size: 10px; wrap: word-wrap; }
            HorizontalBox {
                Button { text: "Start"; clicked => { root.start(); } }
                Button { text: "Stop"; clicked => { root.stop(); } }
                Button { text: "Flip"; clicked => { root.flip(); } }
                Button { text: "Rot"; clicked => { root.rotate(); } }
                Button { text: "Mir"; clicked => { root.mirror(); } }
            }
        }
    }
}

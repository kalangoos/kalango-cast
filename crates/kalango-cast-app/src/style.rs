//! The window's look: the KalangoOS Design System's dark tokens (tokens-dark.css of kalangoos/design), Inter, no
//! shadows, radii 8 (panels, windows) / 6 (cards, toggles) / 4 (controls); circles only for dots and switches.
//! A light mode follows when the app learns the system's choice (today the app is always dark).

pub const CSS: &str = r#"
@define-color k_accent #A5A1E2;
@define-color k_accent_fg #1C1D24;
@define-color k_bg #16171B;
@define-color k_border #26272F;
@define-color k_control #24252D;
@define-color k_control2 #202027;
@define-color k_dim #7A7B88;
@define-color k_divider #2E2F3A;
@define-color k_field #1A1B21;
@define-color k_muted #8E8F99;
@define-color k_panel #101114;
@define-color k_raised #2A2B35;
@define-color k_red #E06C75;
@define-color k_surface #1C1D23;
@define-color k_surface2 #22232B;
@define-color k_teal #8ED2C3;
@define-color k_gold #E5C98C;
@define-color k_knob #DCD3D3;
@define-color k_text #E2E2E6;
@define-color k_text2 #B9BAC4;

window.cast, window.cast-dialog { background: @k_bg; color: @k_text; font-family: "Inter", "Inter Variable", sans-serif; font-size: 13px; box-shadow: none; border-radius: 8px; }
window.cast headerbar { background: @k_panel; color: @k_text; box-shadow: none; border: none; border-bottom: 1px solid @k_border; min-height: 40px; padding: 0 8px; }
window.cast headerbar label.title { font-weight: 600; font-size: 13px; }
window.cast headerbar button, window.cast headerbar windowcontrols button { box-shadow: none; background: transparent; border: none; border-radius: 4px; min-width: 28px; min-height: 28px; padding: 0; color: @k_text2; }
window.cast headerbar button:hover { background: @k_control; }

.topbar { background: @k_panel; border-bottom: 1px solid @k_border; padding: 10px 16px; }
.topbar .state { font-weight: 600; font-size: 14px; }
.topbar .timer { font-family: "Inter", sans-serif; font-feature-settings: "tnum"; font-weight: 600; font-size: 20px; }
.topbar .timer.recording { color: @k_red; }
.chip { background: @k_surface; border: 1px solid @k_border; border-radius: 4px; padding: 3px 8px; color: @k_text2; font-size: 12px; }
.chip.ok { color: @k_teal; }
.chip.bad { color: @k_red; }

.banner { background: @k_surface2; border: 1px solid @k_border; border-radius: 6px; padding: 8px 12px; margin: 12px 16px 0 16px; }
.banner.error { border-color: @k_red; }
.banner.error label { color: @k_red; }

.sidebar { background: @k_panel; border-right: 1px solid @k_border; }
.section-title { font-size: 11px; font-weight: 600; color: @k_muted; letter-spacing: 0.6px; }
.card { background: @k_surface; border: 1px solid @k_border; border-radius: 8px; padding: 14px; }
.card .card-title { font-weight: 600; font-size: 14px; }
.dim { color: @k_muted; }
.mono { font-family: "JetBrains Mono", "DejaVu Sans Mono", monospace; font-size: 12px; }

button { box-shadow: none; background-image: none; background: @k_control; color: @k_text; border: 1px solid @k_border; border-radius: 4px; padding: 5px 12px; min-height: 20px; outline: none; }
button:hover { background: @k_raised; }
button:disabled { color: @k_dim; background: @k_control2; }
button.record { background: @k_accent; color: @k_accent_fg; border-color: @k_accent; font-weight: 600; padding: 7px 18px; }
button.record:disabled { background: @k_control2; color: @k_dim; border-color: @k_border; }
button.record.stop { background: @k_red; border-color: @k_red; color: @k_accent_fg; }
button.flat-icon { background: transparent; border-color: transparent; padding: 3px 6px; }

button.scene { background: @k_surface; border: 1px solid @k_border; border-radius: 6px; padding: 10px 12px; }
button.scene:hover { background: @k_surface2; }
button.scene:checked { background: @k_surface2; border-color: @k_accent; }
button.scene:disabled { color: @k_dim; }
button.scene .scene-title { font-weight: 600; }

list.sources { background: transparent; }
list.sources row { background: @k_surface; border: 1px solid @k_border; border-radius: 6px; padding: 8px 10px; margin: 0 0 6px 0; }
list.sources row:selected { background: @k_surface2; border-color: @k_accent; color: @k_text; }
list.sources row:hover { background: @k_surface2; }

levelbar trough { background: @k_control; border: none; border-radius: 4px; min-height: 8px; }
levelbar block.filled { background: @k_accent; border-radius: 4px; border: none; }
levelbar block.empty { background: transparent; border: none; }

switch { background: @k_control; border: 1px solid @k_border; border-radius: 99px; box-shadow: none; }
switch:checked { background: @k_accent; }
switch slider { background: @k_knob; box-shadow: none; border-radius: 99px; }

window.cast-dialog .what { background: @k_field; border: 1px solid @k_border; border-radius: 6px; padding: 10px 12px; }
window.cast-dialog .warn { background: @k_surface2; border-radius: 6px; padding: 8px 12px; color: @k_text2; }
scrolledwindow undershoot, scrolledwindow overshoot { background: none; }
"#;

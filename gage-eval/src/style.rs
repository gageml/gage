use tabled::settings::Color;

pub fn dim() -> Color {
    Color::new("\x1b[2m", "\x1b[0m")
}

pub fn dim_italic() -> Color {
    Color::new("\x1b[2;3m", "\x1b[22;23m")
}

struct NotDisplay;

fn needs_display<T: std::fmt::Display>(value: T) -> String {
    format!("{value}")
}

pub fn render_value() -> String {
    needs_display(NotDisplay)
}

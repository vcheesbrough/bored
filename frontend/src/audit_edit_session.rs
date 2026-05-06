//! Groups autosaved card PUTs during one editing burst into one server audit row.

pub fn new() -> String {
    let now = js_sys::Date::now();
    let r = (js_sys::Math::random() * 4_294_967_296.0) as u32;
    format!("aes-{now:.0}-{r:x}")
}

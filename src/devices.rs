//! Device presets for `open_browser_session({ device })`: a phone, tablet or
//! desktop's screen size, pixel ratio, User-Agent and whether it's a touch
//! device, so a check on "an iPhone 15" doesn't need its numbers looked up.

/// One preset. `user_agent` may hold `{chrome}`, replaced with the running
/// Chrome's major version so a Chrome User-Agent matches the browser.
pub struct Device {
    pub name: &'static str,
    pub width: i64,
    pub height: i64,
    pub scale: f64,
    /// A phone or tablet: touch screen and mobile device metrics.
    pub mobile: bool,
    /// `None` keeps Chrome's own (desktop) User-Agent.
    pub user_agent: Option<&'static str>,
}

const SAFARI_IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1";

pub const DEVICES: &[Device] = &[
    Device {
        name: "pixel-7",
        width: 412,
        height: 915,
        scale: 2.625,
        mobile: true,
        user_agent: Some(
            "Mozilla/5.0 (Linux; Android 14; Pixel 7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{chrome}.0.0.0 Mobile Safari/537.36",
        ),
    },
    Device {
        name: "galaxy-s23",
        width: 360,
        height: 780,
        scale: 3.0,
        mobile: true,
        user_agent: Some(
            "Mozilla/5.0 (Linux; Android 14; SM-S911B) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{chrome}.0.0.0 Mobile Safari/537.36",
        ),
    },
    Device {
        name: "iphone-15",
        width: 393,
        height: 852,
        scale: 3.0,
        mobile: true,
        user_agent: Some(SAFARI_IPHONE),
    },
    Device {
        name: "iphone-se",
        width: 375,
        height: 667,
        scale: 2.0,
        mobile: true,
        user_agent: Some(SAFARI_IPHONE),
    },
    Device {
        name: "ipad-air",
        width: 820,
        height: 1180,
        scale: 2.0,
        mobile: true,
        // iPadOS Safari presents as a Mac by default.
        user_agent: Some(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1.15",
        ),
    },
    Device {
        name: "laptop",
        width: 1440,
        height: 900,
        scale: 2.0,
        mobile: false,
        user_agent: None,
    },
    Device {
        name: "desktop-1080p",
        width: 1920,
        height: 1080,
        scale: 1.0,
        mobile: false,
        user_agent: None,
    },
];

pub fn find(name: &str) -> Option<&'static Device> {
    DEVICES.iter().find(|d| d.name == name)
}

pub fn names() -> String {
    DEVICES
        .iter()
        .map(|d| d.name)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_found() {
        for d in DEVICES {
            assert_eq!(find(d.name).map(|f| f.name), Some(d.name));
            assert_eq!(
                DEVICES.iter().filter(|o| o.name == d.name).count(),
                1,
                "{}",
                d.name
            );
        }
        assert!(find("nokia-3310").is_none());
    }
}

//! Border/accent color scheme for the client chrome. Maps a pure `amux_core::config::Profile`
//! to the ratatui `Color`s the render functions use, so concurrent sessions can wear distinct
//! colors. `focus` replaces the former `Color::Cyan` accent (focused borders, selection,
//! markers, status chips); `shell` replaces the former `Color::Blue` secondary-pane border.
//! Agent *state* colors (`color_for`) are semantic and unaffected.

use amux_core::config::Profile;
use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub focus: Color,
    pub shell: Color,
}

impl Theme {
    pub fn for_profile(profile: Profile) -> Self {
        match profile {
            Profile::Blue => Theme {
                focus: Color::Cyan,
                shell: Color::Blue,
            },
            Profile::Green => Theme {
                focus: Color::LightGreen,
                shell: Color::Green,
            },
            Profile::Yellow => Theme {
                focus: Color::LightYellow,
                shell: Color::Rgb(180, 140, 0),
            },
            Profile::Red => Theme {
                focus: Color::LightRed,
                shell: Color::Red,
            },
        }
    }
}

impl Default for Theme {
    /// The original scheme — cyan focus, blue shell.
    fn default() -> Self {
        Theme::for_profile(Profile::Blue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_map_to_expected_colors() {
        assert_eq!(
            Theme::for_profile(Profile::Blue),
            Theme {
                focus: Color::Cyan,
                shell: Color::Blue
            }
        );
        assert_eq!(
            Theme::for_profile(Profile::Green),
            Theme {
                focus: Color::LightGreen,
                shell: Color::Green
            }
        );
        assert_eq!(
            Theme::for_profile(Profile::Yellow),
            Theme {
                focus: Color::LightYellow,
                shell: Color::Rgb(180, 140, 0)
            }
        );
        assert_eq!(
            Theme::for_profile(Profile::Red),
            Theme {
                focus: Color::LightRed,
                shell: Color::Red
            }
        );
        assert_eq!(Theme::default(), Theme::for_profile(Profile::Blue));
    }
}

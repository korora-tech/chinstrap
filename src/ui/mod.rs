//! GPUI front end for the recording studio (SPEC §4.2–§4.4).

pub mod shell;
pub mod studio_view;
pub mod theme;
pub mod widgets;

use gpui::actions;

actions!(
    chinstrap,
    [ToggleRecord, Cut, Quit, ShowStudio, ShowExplorer]
);

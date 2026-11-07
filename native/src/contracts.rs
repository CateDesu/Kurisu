#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub enum Route {
    #[default]
    MyList,
    Now,
    Library,
    Torrents,
    Seasons,
    Calendar,
    Search,
    Stats,
    Notifications,
    Settings,
    Anime(i64),
}

impl Route {
    pub const NAVIGATION: [Self; 10] = [
        Self::MyList,
        Self::Now,
        Self::Library,
        Self::Torrents,
        Self::Seasons,
        Self::Calendar,
        Self::Search,
        Self::Stats,
        Self::Notifications,
        Self::Settings,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Self::MyList => "My List",
            Self::Now => "Currently Watching",
            Self::Library => "Library",
            Self::Torrents => "Torrents",
            Self::Seasons => "Seasons",
            Self::Calendar => "Calendar",
            Self::Search => "Search",
            Self::Stats => "Stats",
            Self::Notifications => "Notifications",
            Self::Settings => "Settings",
            Self::Anime(_) => "Anime details",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloseIntent {
    Window,
    Quit,
}

#[derive(Clone, Debug)]
pub enum WorkspaceEvent {
    StateChanged,
    ReadyToClose(CloseIntent),
    CloseCancelled,
    UpdateInstallerStarted,
}

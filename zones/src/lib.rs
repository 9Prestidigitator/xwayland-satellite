#![allow(non_camel_case_types, non_upper_case_globals)]

pub mod client {
    use wayland_client::{self, protocol::*};
    use wayland_protocols::xdg::shell::client::*;

    pub mod __interfaces {
        use wayland_client::backend as wayland_backend;
        use wayland_client::protocol::__interfaces::*;
        use wayland_protocols::xdg::shell::client::__interfaces::*;

        wayland_scanner::generate_interfaces!("src/xx-zones-v1.xml");
    }

    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("src/xx-zones-v1.xml");
}

pub mod server {
    use wayland_protocols::xdg::shell::server::*;
    use wayland_server::{self, protocol::*};

    pub mod __interfaces {
        use wayland_protocols::xdg::shell::server::__interfaces::*;
        use wayland_server::backend as wayland_backend;
        use wayland_server::protocol::__interfaces::*;

        wayland_scanner::generate_interfaces!("src/xx-zones-v1.xml");
    }

    use self::__interfaces::*;
    wayland_scanner::generate_server_code!("src/xx-zones-v1.xml");
}

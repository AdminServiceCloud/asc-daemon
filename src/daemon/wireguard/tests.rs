//! The WireGuard manager against a fake `wg`.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use super::tool::LivePeer;
use super::tool::fake::FakeWg;
use super::*;
use crate::daemon::exec::ModuleError;

struct Fixture {
    wg: Wireguard,
    tool: Arc<FakeWg>,
    dir: tempfile::TempDir,
}

impl Fixture {
    fn conf(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.path().join("etc").join(format!("{name}.conf"))).unwrap()
    }

    fn conf_path(&self, name: &str) -> PathBuf {
        self.dir.path().join("etc").join(format!("{name}.conf"))
    }
}

fn fixture_with(tool: FakeWg) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        conf_dir: dir.path().join("etc"),
        work: dir.path().join("work"),
    };
    std::fs::create_dir_all(&paths.conf_dir).unwrap();
    let tool = Arc::new(tool);
    let wg = Wireguard::with(paths, Box::new(Arc::clone(&tool)));
    Fixture { wg, tool, dir }
}

fn fixture() -> Fixture {
    fixture_with(FakeWg::default())
}

fn kind(err: &anyhow::Error) -> &'static str {
    match err.downcast_ref::<ModuleError>() {
        Some(ModuleError::Invalid(_)) => "invalid",
        Some(ModuleError::Precondition(_)) => "precondition",
        Some(ModuleError::NotFound(_)) => "not-found",
        None => "other",
    }
}

fn server_input(name: &str, port: u16, address: &str) -> InterfaceInput {
    InterfaceInput {
        name: name.into(),
        listen_port: Some(port),
        addresses: vec![address.into()],
        client_dns: vec!["1.1.1.1".into()],
        endpoint: "vpn.example.com".into(),
        ..Default::default()
    }
}

fn add(interface: &str, name: &str) -> PeerAdd {
    PeerAdd {
        interface: interface.into(),
        name: name.into(),
        persistent_keepalive: 25,
        ..Default::default()
    }
}

fn key(c: char) -> String {
    format!("{}=", c.to_string().repeat(43))
}

fn iface<'a>(o: &'a Overview, name: &str) -> &'a InterfaceView {
    o.interfaces
        .iter()
        .find(|i| i.name == name)
        .expect("interface")
}

#[test]
fn a_node_without_wireguard_says_so_and_refuses_changes() {
    let f = fixture_with(FakeWg {
        missing: true,
        ..FakeWg::default()
    });
    let overview = f.wg.overview().unwrap();
    assert!(!overview.installed && overview.interfaces.is_empty());
    let err =
        f.wg.upsert_interface(server_input("wg0", 51820, "10.8.0.1/24"))
            .unwrap_err();
    assert_eq!(kind(&err), "precondition");
    assert!(err.to_string().contains("asc wireguard install"));
}

#[test]
fn a_new_interface_gets_a_key_a_private_file_and_comes_up() {
    let f = fixture();
    let overview =
        f.wg.upsert_interface(server_input("wg0", 51820, "10.8.0.1/24"))
            .unwrap();
    let text = f.conf("wg0");
    assert!(text.starts_with(MARKER));
    assert!(text.contains("PrivateKey = K") && text.contains("ListenPort = 51820"));
    // The clients' DNS is a note for client configs, never the server's own
    // `DNS =` line: wg-quick would point this host's resolver at it.
    assert!(!text.lines().any(|l| l.starts_with("DNS")), "{text}");
    assert!(text.contains("# asc:client-dns = 1.1.1.1"));
    let mode = std::fs::metadata(f.conf_path("wg0"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(f.tool.calls().last().unwrap(), "up wg0");

    let view = iface(&overview, "wg0");
    assert!(view.running && view.enabled);
    assert!(view.public_key.starts_with('P'));
    // The private key is nowhere in what the API shows.
    let private = text
        .lines()
        .find_map(|l| l.strip_prefix("PrivateKey = "))
        .unwrap();
    assert!(!serde_json::to_string(&overview).unwrap().contains(private));
    // No candidate file is left behind.
    assert!(!f.dir.path().join("work/candidate/wg0.conf").exists());
}

#[test]
fn a_brought_in_private_key_is_kept() {
    let f = fixture();
    let mut input = server_input("wg0", 51820, "10.8.0.1/24");
    input.private_key = key('Q');
    f.wg.upsert_interface(input).unwrap();
    assert!(
        f.conf("wg0")
            .contains(&format!("PrivateKey = {}", key('Q')))
    );
}

#[test]
fn editing_an_interface_syncs_or_rebuilds_depending_on_what_changed() {
    let f = fixture();
    f.wg.upsert_interface(server_input("wg0", 51820, "10.8.0.1/24"))
        .unwrap();
    let key_before = f
        .conf("wg0")
        .lines()
        .find(|l| l.starts_with("PrivateKey"))
        .unwrap()
        .to_string();
    f.tool.clear_calls();

    // The port and the public endpoint do not need a rebuild.
    let mut input = server_input("wg0", 51821, "10.8.0.1/24");
    input.endpoint = "vpn2.example.com".into();
    f.wg.upsert_interface(input).unwrap();
    let calls = f.tool.calls();
    assert!(
        calls.iter().any(|c| c == "syncconf wg0")
            && !calls.iter().any(|c| c.starts_with("restart")),
        "{calls:?}"
    );
    assert!(f.conf("wg0").contains("ListenPort = 51821"));
    assert!(
        f.conf("wg0").contains(&key_before),
        "the key survives an edit"
    );

    // An address does.
    f.tool.clear_calls();
    f.wg.upsert_interface(server_input("wg0", 51821, "10.9.0.1/24"))
        .unwrap();
    assert!(f.tool.calls().iter().any(|c| c == "restart wg0"));
}

#[test]
fn two_interfaces_cannot_share_a_listen_port() {
    let f = fixture();
    f.wg.upsert_interface(server_input("wg0", 51820, "10.8.0.1/24"))
        .unwrap();
    let err =
        f.wg.upsert_interface(server_input("wg1", 51820, "10.9.0.1/24"))
            .unwrap_err();
    assert_eq!(kind(&err), "precondition");
    assert!(err.to_string().contains("wg0"));
    assert!(!f.conf_path("wg1").exists());
}

#[test]
fn invalid_input_is_refused_before_anything_is_written() {
    let f = fixture();
    for bad in [
        server_input("../etc", 51820, "10.8.0.1/24"),
        server_input("wg0", 51820, "not-an-address"),
        server_input("wg0", 51820, ""),
    ] {
        let err = f.wg.upsert_interface(bad).unwrap_err();
        assert!(matches!(kind(&err), "invalid"), "{err:#}");
    }
    assert!(f.wg.conf_names().is_empty());
}

#[test]
fn an_interface_that_will_not_come_up_leaves_no_file() {
    let f = fixture();
    f.tool.fail("up");
    let err =
        f.wg.upsert_interface(server_input("wg0", 51820, "10.8.0.1/24"))
            .unwrap_err();
    assert_eq!(kind(&err), "precondition");
    assert!(!f.conf_path("wg0").exists());
}

#[test]
fn a_peer_gets_the_next_address_keys_and_a_client_config() {
    let f = fixture();
    f.wg.upsert_interface(server_input("wg0", 51820, "10.8.0.1/24"))
        .unwrap();
    let mut request = add("wg0", "phone");
    request.preshared = true;
    let added = f.wg.add_peer(request).unwrap();

    let server = f.conf("wg0");
    assert!(server.contains("# asc:name = phone"));
    assert!(server.contains(&format!("PublicKey = {}", added.public_key)));
    assert!(server.contains("AllowedIPs = 10.8.0.2/32"));
    assert!(server.contains("PresharedKey = S"));
    // The peer's private key is in the config handed out and nowhere on disk.
    let private = added
        .client_config
        .lines()
        .find_map(|l| l.strip_prefix("PrivateKey = "))
        .unwrap();
    assert!(private.starts_with('K') && !server.contains(private));
    assert!(added.client_config.contains("Address = 10.8.0.2/32"));
    assert!(
        added
            .client_config
            .contains("Endpoint = vpn.example.com:51820")
    );
    assert!(added.client_config.contains("AllowedIPs = 10.8.0.0/24"));
    assert!(added.client_config.contains("PresharedKey = S"));

    let second = f.wg.add_peer(add("wg0", "laptop")).unwrap();
    assert!(f.conf("wg0").contains("AllowedIPs = 10.8.0.3/32"));
    assert_ne!(second.public_key, added.public_key);
    // Adding a peer to a running tunnel does not rebuild it.
    assert!(!f.tool.calls().iter().any(|c| c == "restart wg0"));
    assert!(f.tool.calls().iter().any(|c| c == "syncconf wg0"));
}

#[test]
fn a_peer_with_its_own_key_gets_a_placeholder_instead_of_a_private_key() {
    let f = fixture();
    f.wg.upsert_interface(server_input("wg0", 51820, "10.8.0.1/24"))
        .unwrap();
    let mut request = add("wg0", "byo");
    request.public_key = key('B');
    request.allowed_ips = vec!["10.8.0.50".into()];
    request.client.routes = "full".into();
    let added = f.wg.add_peer(request).unwrap();
    assert!(added.client_config.contains("PrivateKey = <PRIVATE_KEY>"));
    assert!(added.client_config.contains("Address = 10.8.0.50/32"));
    assert!(added.client_config.contains("AllowedIPs = 0.0.0.0/0, ::/0"));
}

#[test]
fn a_site_to_site_peer_is_added_without_a_client_config_and_rebuilds_for_its_routes() {
    let f = fixture();
    f.wg.upsert_interface(server_input("wg0", 51820, "10.8.0.1/24"))
        .unwrap();
    f.tool.clear_calls();
    let mut request = add("wg0", "branch");
    request.public_key = key('B');
    request.allowed_ips = vec!["192.168.50.0/24".into()];
    let added = f.wg.add_peer(request).unwrap();
    assert_eq!(added.client_config, "");
    // wg-quick installs the route for 192.168.50.0/24 only when it brings the interface up.
    assert!(f.tool.calls().iter().any(|c| c == "restart wg0"));
}

#[test]
fn a_peers_allowed_ips_can_be_edited_and_a_duplicate_address_is_refused() {
    let f = fixture();
    f.wg.upsert_interface(server_input("wg0", 51820, "10.8.0.1/24"))
        .unwrap();
    let a = f.wg.add_peer(add("wg0", "a")).unwrap();
    let b = f.wg.add_peer(add("wg0", "b")).unwrap();
    f.tool.clear_calls();

    let update = |peer: &str, ips: &[&str]| PeerUpdate {
        interface: "wg0".into(),
        peer: peer.into(),
        name: "a".into(),
        allowed_ips: ips.iter().map(|s| s.to_string()).collect(),
        persistent_keepalive: 25,
        enabled: true,
        ..Default::default()
    };
    // Another peer's address: refused, and the file is unchanged.
    let before = f.conf("wg0");
    let err =
        f.wg.update_peer(update(&a.public_key, &["10.8.0.3"]))
            .unwrap_err();
    assert_eq!(kind(&err), "invalid");
    assert_eq!(f.conf("wg0"), before);

    // Same host address, plus the network behind the peer.
    f.wg.update_peer(update(&a.public_key, &["10.8.0.2/32", "172.16.0.0/16"]))
        .unwrap();
    assert!(
        f.conf("wg0")
            .contains("AllowedIPs = 10.8.0.2/32, 172.16.0.0/16")
    );
    assert!(f.tool.calls().iter().any(|c| c == "restart wg0"));

    // A rename alone is a sync.
    f.tool.clear_calls();
    let mut rename = update("a", &["10.8.0.2/32", "172.16.0.0/16"]);
    rename.name = "alpha".into();
    f.wg.update_peer(rename).unwrap();
    assert!(f.conf("wg0").contains("# asc:name = alpha"));
    assert_eq!(
        f.tool
            .calls()
            .iter()
            .filter(|c| c.starts_with("restart"))
            .count(),
        0
    );

    let err = f.wg.update_peer(update(&b.public_key, &[])).unwrap_err();
    assert_eq!(kind(&err), "invalid");
    assert!(err.to_string().contains("at least one"));
}

#[test]
fn a_peer_can_be_switched_off_and_on_without_losing_its_entry() {
    let f = fixture();
    f.wg.upsert_interface(server_input("wg0", 51820, "10.8.0.1/24"))
        .unwrap();
    let a = f.wg.add_peer(add("wg0", "phone")).unwrap();
    let toggle = |enabled: bool| PeerUpdate {
        interface: "wg0".into(),
        peer: "phone".into(),
        name: "phone".into(),
        allowed_ips: vec!["10.8.0.2/32".into()],
        persistent_keepalive: 25,
        enabled,
        ..Default::default()
    };
    let overview = f.wg.update_peer(toggle(false)).unwrap();
    let peer = &iface(&overview, "wg0").peers[0];
    assert!(!peer.peer.enabled && peer.peer.public_key == a.public_key);
    assert!(!f.conf("wg0").lines().any(|l| l.starts_with("PublicKey")));
    // The 10.8.0.2 address stays reserved for it.
    assert!(f.wg.add_peer(add("wg0", "next")).is_ok());
    assert!(f.conf("wg0").contains("AllowedIPs = 10.8.0.3/32"));
    let overview = f.wg.update_peer(toggle(true)).unwrap();
    assert!(iface(&overview, "wg0").peers[0].peer.enabled);
    assert!(f.conf("wg0").lines().any(|l| l.starts_with("PublicKey")));
}

#[test]
fn a_peer_is_found_by_name_and_removed() {
    let f = fixture();
    f.wg.upsert_interface(server_input("wg0", 51820, "10.8.0.1/24"))
        .unwrap();
    f.wg.add_peer(add("wg0", "phone")).unwrap();
    f.wg.add_peer(add("wg0", "laptop")).unwrap();
    let overview = f.wg.remove_peer("wg0", "phone").unwrap();
    let names: Vec<&str> = iface(&overview, "wg0")
        .peers
        .iter()
        .map(|p| p.peer.name.as_str())
        .collect();
    assert_eq!(names, ["laptop"]);
    assert_eq!(
        kind(&f.wg.remove_peer("wg0", "phone").unwrap_err()),
        "not-found"
    );
    assert_eq!(
        kind(&f.wg.remove_peer("nope", "x").unwrap_err()),
        "not-found"
    );
}

#[test]
fn the_client_config_of_an_existing_peer_has_a_placeholder_key_and_chosen_routes() {
    let f = fixture();
    f.wg.upsert_interface(server_input("wg0", 51820, "10.8.0.1/24"))
        .unwrap();
    f.wg.add_peer(add("wg0", "phone")).unwrap();
    let client = ClientInput {
        routes: "10.8.0.0/24, 192.168.10.0/24".into(),
        dns: vec!["9.9.9.9".into()],
        endpoint: "203.0.113.1".into(),
    };
    let config = f.wg.peer_config("wg0", "phone", &client).unwrap();
    assert!(config.contains("PrivateKey = <PRIVATE_KEY>"));
    assert!(config.contains("AllowedIPs = 10.8.0.0/24, 192.168.10.0/24"));
    assert!(config.contains("DNS = 9.9.9.9") && config.contains("Endpoint = 203.0.113.1:51820"));
    let err =
        f.wg.peer_config(
            "wg0",
            "phone",
            &ClientInput {
                routes: "300.0.0.0/8".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert_eq!(kind(&err), "invalid");
}

#[test]
fn a_change_wireguard_refuses_restores_the_previous_file() {
    let f = fixture();
    f.wg.upsert_interface(server_input("wg0", 51820, "10.8.0.1/24"))
        .unwrap();
    let before = f.conf("wg0");

    f.tool.fail("syncconf");
    let err = f.wg.add_peer(add("wg0", "phone")).unwrap_err();
    assert_eq!(kind(&err), "precondition");
    assert!(
        err.to_string()
            .contains("previous configuration was restored")
    );
    assert_eq!(f.conf("wg0"), before);
    f.tool.heal();

    // A file wg-quick will not even read never reaches the real one.
    f.tool.fail("strip");
    let err = f.wg.add_peer(add("wg0", "phone")).unwrap_err();
    assert_eq!(kind(&err), "invalid");
    assert_eq!(f.conf("wg0"), before);
}

#[test]
fn an_interface_can_be_taken_down_brought_up_and_removed() {
    let f = fixture();
    f.wg.upsert_interface(server_input("wg0", 51820, "10.8.0.1/24"))
        .unwrap();
    let overview = f.wg.set_state("wg0", false).unwrap();
    assert!(!iface(&overview, "wg0").running && !iface(&overview, "wg0").enabled);
    let overview = f.wg.set_state("wg0", true).unwrap();
    assert!(iface(&overview, "wg0").running && iface(&overview, "wg0").enabled);
    assert_eq!(
        kind(&f.wg.set_state("nope", true).unwrap_err()),
        "not-found"
    );
    let overview = f.wg.remove_interface("wg0").unwrap();
    assert!(overview.interfaces.is_empty() && !f.conf_path("wg0").exists());
    assert_eq!(
        kind(&f.wg.remove_interface("wg0").unwrap_err()),
        "not-found"
    );
}

#[test]
fn live_data_is_merged_into_the_peers_of_a_running_interface() {
    let f = fixture();
    f.wg.upsert_interface(server_input("wg0", 51820, "10.8.0.1/24"))
        .unwrap();
    let a = f.wg.add_peer(add("wg0", "phone")).unwrap();
    f.wg.add_peer(add("wg0", "idle")).unwrap();
    f.tool.set_live(
        "wg0",
        vec![LivePeer {
            public_key: a.public_key.clone(),
            endpoint: "203.0.113.9:40000".into(),
            latest_handshake_unix: 1_700_000_000,
            rx_bytes: 10,
            tx_bytes: 20,
        }],
    );
    let overview = f.wg.overview().unwrap();
    let peers = &iface(&overview, "wg0").peers;
    let live = peers[0].live.as_ref().unwrap();
    assert_eq!(
        (live.latest_handshake_unix, live.rx_bytes, live.tx_bytes),
        (1_700_000_000, 10, 20)
    );
    assert!(peers[1].live.is_none());
    // Down: no live data at all.
    f.wg.set_state("wg0", false).unwrap();
    assert!(
        iface(&f.wg.overview().unwrap(), "wg0")
            .peers
            .iter()
            .all(|p| p.live.is_none())
    );
}

#[test]
fn foreign_files_are_listed_viewed_and_switched_but_never_edited() {
    let f = fixture();
    std::fs::write(
        f.conf_path("legacy"),
        format!(
            "[Interface]\nPrivateKey = {}\nAddress = 10.1.0.1/24\n",
            key('Z')
        ),
    )
    .unwrap();
    f.tool.set_running("legacy");
    let overview = f.wg.overview().unwrap();
    assert!(overview.interfaces.is_empty());
    assert_eq!(overview.unmanaged.len(), 1);
    assert!(overview.unmanaged[0].running && overview.unmanaged[0].error.is_empty());

    // Not editable ...
    for err in [
        f.wg.upsert_interface(server_input("legacy", 1, "10.0.0.1/24"))
            .unwrap_err(),
        f.wg.add_peer(add("legacy", "x")).unwrap_err(),
    ] {
        assert_eq!(kind(&err), "precondition");
        assert!(err.to_string().contains("not managed"));
    }
    // ... but it can be looked at, without its key, and switched.
    let shown = f.wg.interface_config("legacy").unwrap();
    assert!(
        shown.contains("Address = 10.1.0.1/24")
            && shown.contains("PrivateKey = (hidden)")
            && !shown.contains(&key('Z'))
    );
    f.wg.set_state("legacy", false).unwrap();
    assert!(f.tool.calls().iter().any(|c| c == "down legacy"));
    assert!(f.conf("legacy").contains(&key('Z')));
}

#[test]
fn the_view_of_a_managed_interface_hides_the_keys() {
    let f = fixture();
    f.wg.upsert_interface(server_input("wg0", 51820, "10.8.0.1/24"))
        .unwrap();
    let mut request = add("wg0", "phone");
    request.preshared = true;
    f.wg.add_peer(request).unwrap();
    let shown = f.wg.interface_config("wg0").unwrap();
    assert!(shown.contains("PrivateKey = (hidden)") && shown.contains("PresharedKey = (hidden)"));
    assert!(!shown.contains("PrivateKey = K") && !shown.contains("PresharedKey = S"));
    assert!(shown.contains("# asc:name = phone"));
    assert_eq!(
        kind(&f.wg.interface_config("nope").unwrap_err()),
        "not-found"
    );
    assert_eq!(kind(&f.wg.interface_config("../x").unwrap_err()), "invalid");
}

#[test]
fn a_file_with_our_marker_that_cannot_be_read_is_reported_not_hidden() {
    let f = fixture();
    std::fs::write(
        f.conf_path("broken"),
        format!("{MARKER}\n[Interface]\nnonsense\n"),
    )
    .unwrap();
    let overview = f.wg.overview().unwrap();
    assert!(overview.interfaces.is_empty());
    assert!(overview.unmanaged[0].error.contains("line 3"));
}

// ── Import ──────────────────────────────────────────────────────────────────

fn server_file() -> String {
    format!(
        "[Interface]\nPrivateKey = {}\nAddress = 10.0.0.1/24\nListenPort = 4000\nPostUp = iptables -A FORWARD -i %i -j ACCEPT\nSaveConfig = true\n\n[Peer]\nPublicKey = {}\nAllowedIPs = 10.0.0.2/32\n",
        key('P'),
        key('A')
    )
}

fn import(name: &str, text: &str) -> ImportInput {
    ImportInput {
        name: name.into(),
        text: text.into(),
        ..Default::default()
    }
}

#[test]
fn a_file_that_runs_commands_needs_an_acknowledgement() {
    let f = fixture();
    let err = f.wg.import(import("office", &server_file())).unwrap_err();
    assert_eq!(kind(&err), "precondition");
    assert!(err.to_string().contains("iptables -A FORWARD"), "{err}");
    assert!(!f.conf_path("office").exists());

    let mut input = import("office", &server_file());
    input.accept_hooks = true;
    let done = f.wg.import(input).unwrap();
    assert_eq!(done.name, "office");
    assert!(done.warnings.iter().any(|w| w.contains("SaveConfig")));
    let text = f.conf("office");
    assert!(text.starts_with(MARKER) && !text.contains("SaveConfig"));
    assert!(text.contains("PostUp = iptables -A FORWARD -i %i -j ACCEPT"));
    assert!(text.contains(&format!("PrivateKey = {}", key('P'))));
    // Not started unless asked.
    assert!(!iface(&done.overview, "office").running);
    // The hooks are shown to the operator.
    assert_eq!(
        iface(&done.overview, "office").hooks,
        ["PostUp: iptables -A FORWARD -i %i -j ACCEPT"]
    );
}

#[test]
fn an_imported_server_file_keeps_its_peers_and_can_start() {
    let f = fixture();
    let plain = server_file().replace("PostUp = iptables -A FORWARD -i %i -j ACCEPT\n", "");
    let mut input = import("office", &plain);
    input.start = true;
    let done = f.wg.import(input).unwrap();
    let view = iface(&done.overview, "office");
    assert!(view.running && view.enabled);
    assert_eq!(view.listen_port, Some(4000));
    assert_eq!(view.peers.len(), 1);
    assert_eq!(view.peers[0].peer.allowed_ips, ["10.0.0.2/32"]);
    // From now on it is edited like any other.
    f.wg.add_peer(add("office", "new")).unwrap();
    assert!(f.conf("office").contains("AllowedIPs = 10.0.0.3/32"));
}

#[test]
fn an_imported_client_file_is_a_tunnel_that_does_not_listen() {
    let f = fixture();
    let text = format!(
        "[Interface]\nPrivateKey = {}\nAddress = 10.66.66.2/32\nDNS = 10.66.66.1\n\n[Peer]\nPublicKey = {}\nPresharedKey = {}\nEndpoint = vpn.example.net:51820\nAllowedIPs = 0.0.0.0/0, ::/0\nPersistentKeepalive = 25\n",
        key('C'),
        key('D'),
        key('E')
    );
    let mut input = import("provider", &text);
    input.start = true;
    let done = f.wg.import(input).unwrap();
    let view = iface(&done.overview, "provider");
    assert_eq!(view.listen_port, None);
    assert_eq!(view.peers[0].peer.endpoint, "vpn.example.net:51820");
    assert_eq!(view.peers[0].peer.allowed_ips, ["0.0.0.0/0", "::/0"]);
    assert!(view.peers[0].has_preshared_key && view.running);
    assert!(
        f.conf("provider")
            .contains("Endpoint = vpn.example.net:51820")
    );
}

#[test]
fn importing_over_an_existing_interface_needs_overwrite_and_keeps_a_backup() {
    let f = fixture();
    let plain = server_file().replace("PostUp = iptables -A FORWARD -i %i -j ACCEPT\n", "");
    f.wg.import(import("office", &plain)).unwrap();
    let original = f.conf("office");

    let again = plain.replace("4000", "4001");
    let err = f.wg.import(import("office", &again)).unwrap_err();
    assert_eq!(kind(&err), "precondition");
    assert_eq!(f.conf("office"), original);

    let mut input = import("office", &again);
    input.overwrite = true;
    f.wg.import(input).unwrap();
    assert!(f.conf("office").contains("ListenPort = 4001"));
    let backup = std::fs::read_to_string(f.dir.path().join("etc/office.conf.asc-bak")).unwrap();
    assert_eq!(backup, original);
}

#[test]
fn an_import_is_checked_like_any_other_change() {
    let f = fixture();
    // Not a WireGuard file at all.
    assert_eq!(
        kind(&f.wg.import(import("x", "hello")).unwrap_err()),
        "invalid"
    );
    // A key that is not a key.
    let bad = server_file()
        .replace(&key('P'), "tooshort=")
        .replace("PostUp = iptables -A FORWARD -i %i -j ACCEPT\n", "");
    let err = f.wg.import(import("x", &bad)).unwrap_err();
    assert_eq!(kind(&err), "invalid");
    assert!(err.to_string().contains("private key"));
    // A name that is not a name.
    assert_eq!(
        kind(&f.wg.import(import("", &server_file())).unwrap_err()),
        "invalid"
    );
    // A port another tunnel already listens on.
    f.wg.upsert_interface(server_input("wg0", 4000, "10.8.0.1/24"))
        .unwrap();
    let plain = server_file().replace("PostUp = iptables -A FORWARD -i %i -j ACCEPT\n", "");
    assert_eq!(
        kind(&f.wg.import(import("office", &plain)).unwrap_err()),
        "precondition"
    );
    assert!(!f.conf_path("office").exists());
}

#[test]
fn an_imported_file_can_be_taken_over_from_an_unmanaged_one() {
    let f = fixture();
    let plain = server_file().replace("PostUp = iptables -A FORWARD -i %i -j ACCEPT\n", "");
    std::fs::write(f.conf_path("office"), &plain).unwrap();
    assert_eq!(f.wg.overview().unwrap().unmanaged.len(), 1);
    let mut input = import("office", &plain);
    input.overwrite = true;
    f.wg.import(input).unwrap();
    let overview = f.wg.overview().unwrap();
    assert!(overview.unmanaged.is_empty() && overview.interfaces.len() == 1);
}

#[test]
fn nat_adds_and_removes_the_masquerade_hooks() {
    let f = fixture();
    let mut input = server_input("wg0", 51820, "10.8.0.1/24");
    input.masquerade = true;
    f.wg.upsert_interface(input).unwrap();
    let text = f.conf("wg0");
    assert!(text.contains("# asc:masquerade") && text.contains("ip saddr 10.8.0.0/24"));
    assert!(iface(&f.wg.overview().unwrap(), "wg0").masquerade);
    // The hooks are the daemon's: they are not listed as the operator's.
    assert!(iface(&f.wg.overview().unwrap(), "wg0").hooks.is_empty());

    f.tool.clear_calls();
    f.wg.upsert_interface(server_input("wg0", 51820, "10.8.0.1/24"))
        .unwrap();
    assert!(!f.conf("wg0").contains("nft"));
    // The hooks changed, so the interface was rebuilt to run them.
    assert!(f.tool.calls().iter().any(|c| c == "restart wg0"));
}

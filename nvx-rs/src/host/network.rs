//! Translation of a network posture into OpenVMM options.
//!
//! OpenVMM's portable profile enforces an egress default plus IPv4 and IPv6 allow and deny rules
//! that match a network of one family and, optionally, one protocol (TCP, UDP, or ICMP) and, for
//! TCP or UDP, one port or an inclusive range of ports; deny rules take precedence. [`EgressRule`]s
//! are expanded into that form exactly: exceptions are subtracted from their networks, a rule
//! without destinations matches `0.0.0.0/0` and `::/0`, and `any` protocol with a port or a port
//! range becomes a TCP and a UDP rule. Rules that cannot be expressed exactly are rejected.

use crate::error::{Error, Result};
use crate::net::cidr::Cidr;

/// Largest number of allow rules, and of deny rules, that OpenVMM accepts.
pub const MAX_RULES: usize = 256;

/// Allow or deny decision of a network posture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Access {
    /// Permit the traffic.
    Allow,
    /// Block the traffic.
    Deny,
}

impl Access {
    /// Returns OpenVMM's spelling of this decision.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

/// Transport protocol of a [`PortSelector`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Protocol {
    /// TCP.
    Tcp,
    /// UDP.
    Udp,
    /// ICMP, or ICMPv6 for IPv6 networks.
    Icmp,
    /// Any protocol, or TCP and UDP with a port.
    #[default]
    Any,
}

/// A destination network of an [`EgressRule`], without its exceptions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    /// Destination address or CIDR, such as `"10.0.0.0/8"` or `"192.0.2.1"`.
    pub cidr: String,
    /// Sub-networks of `cidr` that the rule does not match.
    pub except: Vec<String>,
}

impl Destination {
    /// Creates a destination for `cidr` without exceptions.
    pub fn new(cidr: impl Into<String>) -> Self {
        Self {
            cidr: cidr.into(),
            except: Vec::new(),
        }
    }
}

/// A destination protocol and port range of an [`EgressRule`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortSelector {
    /// Transport protocol.
    pub protocol: Protocol,
    /// First destination port. `None` matches every port.
    pub port: Option<u16>,
    /// Last destination port of a range that starts at `port`.
    pub end_port: Option<u16>,
}

/// Traffic to some destinations and ports.
///
/// An empty `to` matches every destination of both families, and an empty `ports` matches every
/// protocol and port.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EgressRule {
    /// Destination networks.
    pub to: Vec<Destination>,
    /// Destination protocols and ports.
    pub ports: Vec<PortSelector>,
}

impl EgressRule {
    /// Creates a rule that matches every protocol and port of `cidr`, such as `"10.0.0.0/8"` or
    /// `"192.0.2.1"`.
    pub fn to(cidr: impl Into<String>) -> Self {
        Self {
            to: vec![Destination::new(cidr)],
            ports: Vec::new(),
        }
    }

    /// Restricts the rule to one port of `protocol`.
    #[must_use]
    pub fn on_port(mut self, protocol: Protocol, port: u16) -> Self {
        self.ports.push(PortSelector {
            protocol,
            port: Some(port),
            end_port: None,
        });
        self
    }

    /// Restricts the rule to the ports of `protocol` from `port` through `end_port`, both
    /// included.
    #[must_use]
    pub fn on_port_range(mut self, protocol: Protocol, port: u16, end_port: u16) -> Self {
        self.ports.push(PortSelector {
            protocol,
            port: Some(port),
            end_port: Some(end_port),
        });
        self
    }

    /// Restricts the rule to `protocol` on every port.
    #[must_use]
    pub fn on_protocol(mut self, protocol: Protocol) -> Self {
        self.ports.push(PortSelector {
            protocol,
            port: None,
            end_port: None,
        });
        self
    }
}

/// The egress rules of one action, and the name by which messages refer to them, such as
/// `egress.allow`. Messages name a rule by its index, as in `egress.allow[0]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressRules {
    /// The name of the rules in messages.
    pub name: String,
    /// The rules.
    pub rules: Vec<EgressRule>,
}

impl EgressRules {
    /// Creates an empty list of rules that messages call `name`.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            rules: Vec::new(),
        }
    }
}

/// The network posture of a VM.
///
/// A connection matching a `deny` rule is blocked. Otherwise a connection matching an `allow`
/// rule is permitted, and any other connection follows the `egress` default. OpenVMM's portable
/// profile denies ingress and the guest's access to host loopback services.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkPolicy {
    /// Default policy for guest-initiated connections.
    pub egress: Access,
    /// Destinations permitted despite a `deny` default.
    pub allow: EgressRules,
    /// Destinations blocked despite an `allow` default or an `allow` rule.
    pub deny: EgressRules,
    /// Default policy for host-initiated connections, which OpenVMM's portable profile can only
    /// deny; [`openvmm_arguments`] refuses [`Access::Allow`].
    pub ingress: Access,
}

impl NetworkPolicy {
    /// Applies `egress` as the default egress policy, with no rules, and denies ingress. Messages
    /// call the rules `egress.allow` and `egress.deny`.
    pub fn new(egress: Access) -> Self {
        Self {
            egress,
            allow: EgressRules::new("egress.allow"),
            deny: EgressRules::new("egress.deny"),
            ingress: Access::Deny,
        }
    }

    /// Adds a rule that permits matching destinations.
    #[must_use]
    pub fn with_allow(mut self, rule: EgressRule) -> Self {
        self.allow.rules.push(rule);
        self
    }

    /// Adds a rule that blocks matching destinations.
    #[must_use]
    pub fn with_deny(mut self, rule: EgressRule) -> Self {
        self.deny.rules.push(rule);
        self
    }
}

/// Returns the OpenVMM options that enforce `policy` for a guest at `guest_network`.
///
/// Without a policy, or when egress is denied without allow rules, no network device is
/// attached. Otherwise the portable profile applies the egress posture with ingress and host
/// loopback denied.
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) when the policy allows
/// ingress, which the portable profile cannot express, when a rule cannot be expressed exactly,
/// or when the rules of one action expand to more than [`MAX_RULES`] OpenVMM rules.
pub fn openvmm_arguments(
    policy: Option<&NetworkPolicy>,
    guest_network: &str,
) -> Result<Vec<String>> {
    let Some(policy) = policy else {
        return Ok(Vec::new());
    };
    if policy.ingress != Access::Deny {
        return Err(Error::unsupported(
            "ingress must be denied; OpenVMM's portable network profile cannot admit \
             host-initiated connections",
        ));
    }
    if policy.egress == Access::Deny && policy.allow.rules.is_empty() {
        return Ok(Vec::new());
    }
    let mut arguments: Vec<String> = [
        "--net",
        guest_network,
        "--network-profile",
        "portable",
        "--network-egress",
        policy.egress.as_str(),
        "--network-ingress",
        "deny",
        "--host-loopback",
        "deny",
    ]
    .map(str::to_owned)
    .to_vec();
    for (rules, option) in [
        (&policy.allow, "--network-egress-allow"),
        (&policy.deny, "--network-egress-deny"),
    ] {
        let field = &rules.name;
        let mut expanded = Vec::new();
        for (index, rule) in rules.rules.iter().enumerate() {
            expand_rule(rule, &format!("{field}[{index}]"), &mut expanded)?;
        }
        expanded.sort_unstable();
        expanded.dedup();
        if expanded.len() > MAX_RULES {
            return Err(Error::unsupported(format!(
                "{field} expands to {} OpenVMM rules; the openvmm backend accepts at most \
                 {MAX_RULES}",
                expanded.len()
            )));
        }
        for rule in expanded {
            arguments.push(option.to_owned());
            arguments.push(rule);
        }
    }
    Ok(arguments)
}

/// Appends the OpenVMM rules equivalent to `rule`.
fn expand_rule(rule: &EgressRule, field: &str, output: &mut Vec<String>) -> Result<()> {
    let unsupported = |message: String| Err(Error::unsupported(format!("{field}{message}")));
    let mut networks = Vec::new();
    if rule.to.is_empty() {
        networks.extend([Cidr::everything(false), Cidr::everything(true)]);
    }
    for (index, peer) in rule.to.iter().enumerate() {
        let Ok(network) = Cidr::parse(&peer.cidr) else {
            return unsupported(format!(".to[{index}]: the destination is not a network"));
        };
        let mut excluded = Vec::new();
        for value in &peer.except {
            match Cidr::parse(value) {
                Ok(exclusion) if network.contains(exclusion) => excluded.push(exclusion),
                _ => {
                    return unsupported(format!(
                        ".to[{index}].except: an exception is not a network within the destination"
                    ));
                }
            }
        }
        let Some(remainder) = network.subtract(&excluded, MAX_RULES - networks.len()) else {
            return unsupported(format!(
                " covers more than {MAX_RULES} networks after removing its exceptions"
            ));
        };
        networks.extend(remainder);
    }

    let mut selectors = Vec::new();
    if rule.ports.is_empty() {
        selectors.push(Selector::Everything);
    }
    for (index, port) in rule.ports.iter().enumerate() {
        let protocols: &[&'static str] = match port.protocol {
            Protocol::Tcp => &["tcp"],
            Protocol::Udp => &["udp"],
            Protocol::Icmp => &["icmp"],
            Protocol::Any => &["tcp", "udp"],
        };
        let Some(start) = port.port else {
            if port.end_port.is_some() {
                return unsupported(format!(".ports[{index}].endPort requires port"));
            }
            if port.protocol == Protocol::Any {
                selectors.push(Selector::Everything);
            } else {
                selectors.extend(protocols.iter().copied().map(Selector::Protocol));
            }
            continue;
        };
        if port.protocol == Protocol::Icmp {
            return unsupported(format!(".ports[{index}]: ICMP has no ports"));
        }
        let end = port.end_port.unwrap_or(start);
        if end < start {
            return unsupported(format!(".ports[{index}].endPort must not be below port"));
        }
        selectors.extend(
            protocols
                .iter()
                .map(|&protocol| Selector::Ports(protocol, start, end)),
        );
    }

    for network in &networks {
        for selector in &selectors {
            output.push(match *selector {
                Selector::Everything => network.to_string(),
                Selector::Protocol(protocol) => format!("{network}:{protocol}"),
                Selector::Ports(protocol, start, end) if start == end => {
                    format!("{network}:{protocol}:{start}")
                }
                Selector::Ports(protocol, start, end) => {
                    format!("{network}:{protocol}:{start}-{end}")
                }
            });
            if output.len() > MAX_RULES * 4 {
                return unsupported(format!(" expands to more than {MAX_RULES} OpenVMM rules"));
            }
        }
    }
    Ok(())
}

/// The traffic that one OpenVMM rule selects at its destination network.
#[derive(Clone, Copy)]
enum Selector {
    /// Every protocol and port.
    Everything,
    /// Every port of TCP or UDP, or every ICMP message.
    Protocol(&'static str),
    /// The TCP or UDP ports from the first through the last, both included.
    Ports(&'static str, u16, u16),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;

    fn rules(arguments: &[String], option: &str) -> Vec<String> {
        arguments
            .windows(2)
            .filter(|pair| pair[0] == option)
            .map(|pair| pair[1].clone())
            .collect()
    }

    #[test]
    fn default_postures_map_to_portable_profile_options() {
        assert!(openvmm_arguments(None, "10.0.0.2/24").unwrap().is_empty());
        assert!(
            openvmm_arguments(Some(&NetworkPolicy::new(Access::Deny)), "10.0.0.2/24")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            openvmm_arguments(Some(&NetworkPolicy::new(Access::Allow)), "10.0.0.2/24").unwrap(),
            [
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
                "--network-egress",
                "allow",
                "--network-ingress",
                "deny",
                "--host-loopback",
                "deny",
            ]
        );
        // Deny rules under a deny default change nothing, so no device is attached.
        let denied = NetworkPolicy::new(Access::Deny).with_deny(EgressRule::to("1.2.3.4"));
        assert!(
            openvmm_arguments(Some(&denied), "10.0.0.2/24")
                .unwrap()
                .is_empty()
        );
        // The portable profile denies ingress, so a policy that allows it is refused rather than
        // tightened.
        for egress in [Access::Deny, Access::Allow] {
            let mut ingress = NetworkPolicy::new(egress);
            ingress.ingress = Access::Allow;
            let error = openvmm_arguments(Some(&ingress), "10.0.0.2/24").unwrap_err();
            assert_eq!(error.kind(), crate::ErrorKind::Unsupported, "{error}");
        }
    }

    #[test]
    fn rules_expand_exactly() {
        let egress = NetworkPolicy::new(Access::Deny)
            .with_allow(EgressRule {
                to: vec![Destination {
                    cidr: "10.0.0.0/30".to_owned(),
                    except: vec!["10.0.0.1".to_owned()],
                }],
                ports: vec![PortSelector {
                    protocol: Protocol::Tcp,
                    port: Some(80),
                    end_port: Some(81),
                }],
            })
            .with_allow(EgressRule::to("192.0.2.7").on_port(Protocol::Any, 53))
            .with_allow(EgressRule::to("198.51.100.0/24"));
        let arguments = openvmm_arguments(Some(&egress), "10.0.0.2/24").unwrap();
        assert_eq!(
            rules(&arguments, "--network-egress-allow"),
            [
                "10.0.0.0/32:tcp:80-81",
                "10.0.0.2/31:tcp:80-81",
                "192.0.2.7/32:tcp:53",
                "192.0.2.7/32:udp:53",
                "198.51.100.0/24",
            ]
        );
        assert!(has_pair(&arguments, "--network-egress", "deny"));

        let egress = NetworkPolicy::new(Access::Allow).with_deny(EgressRule {
            to: Vec::new(),
            ports: vec![PortSelector {
                protocol: Protocol::Udp,
                port: Some(53),
                end_port: None,
            }],
        });
        let arguments = openvmm_arguments(Some(&egress), "10.0.0.2/24").unwrap();
        // A rule without destinations matches both address families.
        assert_eq!(
            rules(&arguments, "--network-egress-deny"),
            ["0.0.0.0/0:udp:53", "::/0:udp:53"]
        );
    }

    #[test]
    fn ipv6_rules_expand_exactly() {
        let egress = NetworkPolicy::new(Access::Deny)
            .with_allow(EgressRule::to("2001:db8:1::/64").on_port(Protocol::Tcp, 443))
            .with_allow(EgressRule {
                to: vec![Destination {
                    cidr: "2001:db8:2::/126".to_owned(),
                    except: vec!["2001:db8:2::1".to_owned()],
                }],
                ports: vec![PortSelector {
                    protocol: Protocol::Any,
                    port: Some(53),
                    end_port: None,
                }],
            })
            .with_allow(EgressRule::to("192.0.2.0/24").on_protocol(Protocol::Icmp))
            .with_allow(EgressRule::to("::/0").on_protocol(Protocol::Icmp))
            .with_deny(EgressRule::to("2001:db8:1::123"));
        let arguments = openvmm_arguments(Some(&egress), "10.0.0.2/24").unwrap();
        assert_eq!(
            rules(&arguments, "--network-egress-allow"),
            [
                "192.0.2.0/24:icmp",
                "2001:db8:1::/64:tcp:443",
                "2001:db8:2::/128:tcp:53",
                "2001:db8:2::/128:udp:53",
                "2001:db8:2::2/127:tcp:53",
                "2001:db8:2::2/127:udp:53",
                "::/0:icmp",
            ]
        );
        assert_eq!(
            rules(&arguments, "--network-egress-deny"),
            ["2001:db8:1::123/128"]
        );
    }

    #[test]
    fn exclusions_remain_local_to_their_rules() {
        let excluding = |cidr: &str, excluded: &str| EgressRule {
            to: vec![Destination {
                cidr: cidr.to_owned(),
                except: vec![excluded.to_owned()],
            }],
            ports: Vec::new(),
        };
        let egress = NetworkPolicy::new(Access::Deny)
            .with_allow(excluding("192.0.2.0/24", "192.0.2.128/25"))
            .with_allow(EgressRule::to("192.0.2.200"))
            .with_deny(excluding("198.51.100.0/24", "198.51.100.128/25"));
        let arguments = openvmm_arguments(Some(&egress), "10.0.0.2/24").unwrap();

        assert_eq!(
            rules(&arguments, "--network-egress-allow"),
            ["192.0.2.0/25", "192.0.2.200/32"]
        );
        assert_eq!(
            rules(&arguments, "--network-egress-deny"),
            ["198.51.100.0/25"]
        );
    }

    fn has_pair(arguments: &[String], name: &str, value: &str) -> bool {
        arguments
            .windows(2)
            .any(|pair| pair[0] == name && pair[1] == value)
    }

    #[test]
    fn protocols_without_ports_expand_to_protocol_rules() {
        let egress = NetworkPolicy::new(Access::Deny)
            .with_allow(EgressRule::to("192.0.2.7").on_protocol(Protocol::Tcp))
            .with_allow(EgressRule::to("192.0.2.7").on_protocol(Protocol::Udp))
            .with_allow(EgressRule::to("192.0.2.7").on_protocol(Protocol::Icmp))
            .with_allow(EgressRule::to("198.51.100.0/24").on_protocol(Protocol::Any))
            .with_allow(EgressRule::to("203.0.113.9").on_port(Protocol::Any, 443))
            .with_deny(
                EgressRule::to("192.0.2.7")
                    .on_protocol(Protocol::Icmp)
                    .on_port(Protocol::Tcp, 22),
            );
        let arguments = openvmm_arguments(Some(&egress), "10.0.0.2/24").unwrap();
        assert_eq!(
            rules(&arguments, "--network-egress-allow"),
            [
                "192.0.2.7/32:icmp",
                "192.0.2.7/32:tcp",
                "192.0.2.7/32:udp",
                "198.51.100.0/24",
                "203.0.113.9/32:tcp:443",
                "203.0.113.9/32:udp:443",
            ]
        );
        assert_eq!(
            rules(&arguments, "--network-egress-deny"),
            ["192.0.2.7/32:icmp", "192.0.2.7/32:tcp:22"]
        );
    }

    #[test]
    fn port_ranges_expand_to_one_rule_per_network_and_protocol() {
        let egress = NetworkPolicy::new(Access::Deny)
            .with_allow(EgressRule::to("192.0.2.7").on_port_range(Protocol::Tcp, 8000, 8010))
            .with_allow(EgressRule::to("192.0.2.7").on_port_range(Protocol::Udp, 5000, 5010))
            .with_allow(EgressRule::to("198.51.100.0/24").on_port_range(Protocol::Any, 1, 65535))
            // A range of one port is that port's rule.
            .with_allow(EgressRule::to("203.0.113.9").on_port_range(Protocol::Tcp, 443, 443))
            .with_deny(EgressRule::to("192.0.2.7").on_port(Protocol::Tcp, 8005));
        let arguments = openvmm_arguments(Some(&egress), "10.0.0.2/24").unwrap();
        assert_eq!(
            rules(&arguments, "--network-egress-allow"),
            [
                "192.0.2.7/32:tcp:8000-8010",
                "192.0.2.7/32:udp:5000-5010",
                "198.51.100.0/24:tcp:1-65535",
                "198.51.100.0/24:udp:1-65535",
                "203.0.113.9/32:tcp:443",
            ]
        );
        assert_eq!(
            rules(&arguments, "--network-egress-deny"),
            ["192.0.2.7/32:tcp:8005"]
        );
    }

    #[test]
    fn inexpressible_rules_are_rejected() {
        let port = |protocol, port, end_port| EgressRule {
            to: vec![Destination::new("10.0.0.0/8")],
            ports: vec![PortSelector {
                protocol,
                port,
                end_port,
            }],
        };
        for rule in [
            port(Protocol::Icmp, Some(8), None),
            // Structural validation rejects these first, unless the backend is called directly.
            port(Protocol::Tcp, Some(90), Some(80)),
            port(Protocol::Udp, None, Some(80)),
            EgressRule {
                to: (0..300)
                    .map(|index| Destination::new(format!("10.0.{}.0/24", index % 256)))
                    .collect(),
                ports: vec![PortSelector {
                    protocol: Protocol::Tcp,
                    port: Some(1),
                    end_port: Some(2),
                }],
            },
            // 129 separate ranges of both TCP and UDP need 258 OpenVMM rules.
            EgressRule {
                to: vec![Destination::new("10.0.0.0/8")],
                ports: (0..129)
                    .map(|index| PortSelector {
                        protocol: Protocol::Any,
                        port: Some(3 * index + 1),
                        end_port: Some(3 * index + 2),
                    })
                    .collect(),
            },
        ] {
            let egress = NetworkPolicy::new(Access::Deny).with_allow(rule.clone());
            assert_eq!(
                openvmm_arguments(Some(&egress), "10.0.0.2/24")
                    .unwrap_err()
                    .kind(),
                ErrorKind::Unsupported,
                "{rule:?}"
            );
        }
    }

    #[test]
    fn exceptions_that_split_a_network_too_finely_are_rejected() {
        // Without a limit, these exceptions would split the address space into 512 * 23 networks.
        let peer = Destination {
            cidr: "0.0.0.0/0".to_owned(),
            except: (0..512u32)
                .map(|index| std::net::Ipv4Addr::from(index << 23).to_string())
                .collect(),
        };
        let rule = EgressRule {
            to: vec![peer],
            ports: Vec::new(),
        };
        let egress = NetworkPolicy::new(Access::Deny).with_allow(rule);
        let error = openvmm_arguments(Some(&egress), "10.0.0.2/24").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(
            error.message().contains("covers more than 256 networks"),
            "{error}"
        );
    }
}

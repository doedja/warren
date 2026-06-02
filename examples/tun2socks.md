# Whole-device routing through warren (tun2socks)

warren's proxy is per-app by default: only what you point at it uses the pool.
To route an **entire machine or phone** (every TCP and UDP flow, including QUIC,
DNS, and WebRTC media) through one of your nodes, put a tun2socks layer in front
of warren's SOCKS5 endpoint. warren supports SOCKS5 UDP ASSOCIATE, so UDP is
carried, not dropped.

This is a client-side setup. The hub and nodes need nothing extra beyond a
working proxy login (and, for UDP, the UDP relay port reachable; see below).

## What you need

- A reachable warren proxy: `SERVER:8000` (or your published port), with a proxy
  user + password.
- For UDP to traverse: the hub's UDP relay port must be reachable. The relay
  shares the proxy's host:port on UDP, so publish that port for UDP too (e.g. in
  Docker map `HOSTPORT:8000/udp`, and open it in the firewall). TCP-only routing
  works without this; UDP/QUIC/WebRTC need it.
- A tun2socks tool. [hev-socks5-tunnel](https://github.com/heiher/hev-socks5-tunnel)
  is small and supports SOCKS5 UDP associate.

## Example (hev-socks5-tunnel, Linux/macOS)

`config.yaml`:

```yaml
tunnel:
  name: tun0
  mtu: 8500
  ipv4: 198.18.0.1
socks5:
  address: SERVER
  port: 8000
  username: warren
  password: PASSWORD
  udp: udp        # relay UDP over the SOCKS5 UDP associate
```

Run it (as root, since it creates a tun device):

```bash
hev-socks5-tunnel config.yaml
```

Then route traffic into `tun0`. The exact commands are OS-specific; the idea is:

- add a route for the proxy server's IP via your normal gateway (so the tunnel
  itself does not loop), then
- make `tun0` the default route for everything else.

Linux sketch:

```bash
ip route add SERVER_IP via $(ip route show default | awk '{print $3; exit}')
ip route add default dev tun0 metric 1
```

Undo by deleting those routes and stopping the tunnel.

## Notes

- **Pick a device** by pointing the tunnel's username at `warren+name`, or a
  region with `warren-region-COUNTRY`, exactly like the per-app examples.
- **Browsers and WebRTC**: a browser bypasses SOCKS for WebRTC by default (the
  "WebRTC leak"). Routing the whole device via tun2socks captures that UDP too,
  so WebRTC then egresses through your node.
- **Performance**: every packet now crosses client to hub to node, so latency is
  bounded by hub placement. Keep the hub near your nodes for the best result.

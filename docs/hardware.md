# Hardware for always-on residential nodes

A pool node only runs the `warren node` agent, which dials out to the hub and
pipes proxied connections. That is featherweight (target is a few MB RAM idle),
so almost any Linux board works. The real selection criteria are: cheap, small,
low idle power (it runs 24/7), and easy to buy in Indonesia. A node does **not**
host the hub; the hub runs on a server (Coolify).

## Recommendation for Indonesia: Orange Pi Zero 2W / Zero 3

Best price + availability locally. Sold widely on Tokopedia/Shopee.

- Allwinner H618, quad-core A53, 1/2/4 GB LPDDR4, Wi-Fi 5 + BT, microSD.
- Idle ~0.9-1.1 W; runs fine on a 5V/2A phone charger.
- Armbian / Orange Pi OS, Tailscale installs via the official script.
- Indicative price (mid-2025, Tokopedia/Shopee): Rp 180.000-280.000 for the 1-2 GB
  board; full kit with case + PSU usually under Rp 300.000. Consistently cheaper than
  a Raspberry Pi Zero 2 W locally.
- Buying tips: search "Orange Pi Zero 2W" or "Orange Pi Zero 3" + "Armbian"; pick a
  seller with high rating and "garansi"; get 1 GB minimum (2 GB is comfortable headroom).

## Alternatives

| Board | Why pick it | Idle | Notes |
|---|---|---|---|
| **Orange Pi Zero 2W / Zero 3** | cheapest + available in ID | ~0.9-1.1 W | recommended; H618, up to 4 GB |
| **Raspberry Pi Zero 2 W** | best software ecosystem, lowest hassle | ~0.5-0.7 W | ~Rp 350k-480k bare in ID; Wi-Fi only; mature docs |
| **Luckfox Pico (Plus/Pro/Max)** | absolute lowest power + smallest | ~0.2-0.3 W | single-core A7, 128-256 MB RAM, Buildroot Linux; fine for a tiny proxy but more DIY |

## Zero-cost options worth remembering

- **Old Android phone**: free if you have one. Run Tailscale (manual VPN-consent tap, cannot
  be scripted) + gost via Termux. Battery/Doze make it a flaky node; Rota's health-check
  routes around it. Good as a bonus mobile-IP node, not a backbone.
- **Used Android TV box** (very cheap in ID, often under Rp 200k): flash Armbian on supported
  Amlogic models and treat it like an SBC. More variance, but a lot of compute for the money.

## Power and siting

- Each board sips ~1 W, so a 24/7 node costs only a few thousand rupiah per month in
  electricity. The point of multiple nodes is IP diversity: place them on different
  connections (home, office, a relative's house) so the pool spans distinct residential IPs.
- One residential line shared by several boards still yields one egress IP per line, so
  spreading across locations matters more than stacking boards in one spot.

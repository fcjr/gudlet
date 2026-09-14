# Vendored crates

## embassy-usb-synopsys-otg 0.4.0

Patched so that bulk OUT endpoints receive up to 128 packets per transfer.
Upstream arms one 64-byte packet at a time and wakes the reading task for
each, which on this chip costs about 110 µs per packet and caps a full-speed
bulk stream near 550 KB/s. With whole transfers the task wakes once per
kilobyte and the bus runs close to its limit.

Every change is marked with `PATCH`. Endpoint 0 keeps the original behaviour
because the control pipe relies on it. The endpoint OUT buffer passed to
`Driver::new` must be large enough for `max_packet_size * 128` per bulk OUT
endpoint.

Upstream licenses this crate under MIT OR Apache-2.0. The upstream
[MIT notice](embassy-usb-synopsys-otg/LICENSE-MIT) is included with this copy.

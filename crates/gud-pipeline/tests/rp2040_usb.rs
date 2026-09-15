// Compile the actual polling adapter against a fake USB bus on the host.
#[path = "../../../firmware/rp2040/src/gud.rs"]
mod adapter;

fn payload_stall() -> ! {
    panic!("payload stalled or overran its announced boundary")
}

fn wake_panel() {}

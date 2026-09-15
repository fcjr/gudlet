//! RP2040 USB adapter that avoids redundant endpoint-control writes.
use rp2040_hal::{clocks::UsbClock, pac, usb::UsbBus as HalBus};
use usb_device::{
    bus::{PollResult, UsbBus as Bus},
    endpoint::{EndpointAddress, EndpointType},
    Result, UsbDirection,
};

pub struct UsbBus(HalBus);

impl UsbBus {
    pub fn new(
        regs: pac::USBCTRL_REGS,
        ram: pac::USBCTRL_DPRAM,
        clock: UsbClock,
        force_vbus: bool,
        resets: &mut pac::RESETS,
    ) -> Self {
        Self(HalBus::new(regs, ram, clock, force_vbus, resets))
    }
}

impl Bus for UsbBus {
    fn alloc_ep(
        &mut self,
        dir: UsbDirection,
        addr: Option<EndpointAddress>,
        kind: EndpointType,
        size: u16,
        interval: u8,
    ) -> Result<EndpointAddress> {
        self.0.alloc_ep(dir, addr, kind, size, interval)
    }
    fn enable(&mut self) {
        self.0.enable();
    }
    fn reset(&self) {
        self.0.reset();
    }
    fn set_device_address(&self, addr: u8) {
        self.0.set_device_address(addr);
    }
    fn write(&self, addr: EndpointAddress, buf: &[u8]) -> Result<usize> {
        self.0.write(addr, buf)
    }
    fn read(&self, addr: EndpointAddress, buf: &mut [u8]) -> Result<usize> {
        self.0.read(addr, buf)
    }
    fn set_stalled(&self, addr: EndpointAddress, stalled: bool) {
        // usb-device unstalls EP0 OUT after reading every SETUP. The HAL has
        // already armed the data stage then, and its set_stalled uses a full
        // read-modify-write of the buffer control register. Avoid overwriting
        // a concurrent hardware update of FULL/AVAILABLE when STALL is clear.
        if addr.index() == 0 && !stalled && !self.0.is_stalled(addr) {
            return;
        }
        self.0.set_stalled(addr, stalled);
    }
    fn is_stalled(&self, addr: EndpointAddress) -> bool {
        self.0.is_stalled(addr)
    }
    fn suspend(&self) {
        self.0.suspend();
    }
    fn resume(&self) {
        self.0.resume();
    }
    fn poll(&self) -> PollResult {
        self.0.poll()
    }
    fn force_reset(&self) -> Result<()> {
        self.0.force_reset()
    }
    const QUIRK_SET_ADDRESS_BEFORE_STATUS: bool = HalBus::QUIRK_SET_ADDRESS_BEFORE_STATUS;
}

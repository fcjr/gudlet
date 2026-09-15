use super::*;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use usb_device::{
    bus::PollResult,
    device::{UsbDeviceBuilder, UsbVidPid},
    UsbDirection,
};

type Packets = Arc<Mutex<VecDeque<Vec<u8>>>>;
struct Bus(Packets);
impl UsbBus for Bus {
    fn alloc_ep(
        &mut self,
        dir: UsbDirection,
        addr: Option<EndpointAddress>,
        _: EndpointType,
        _: u16,
        _: u8,
    ) -> usb_device::Result<EndpointAddress> {
        Ok(addr.unwrap_or_else(|| EndpointAddress::from_parts(1, dir)))
    }
    fn enable(&mut self) {}
    fn reset(&self) {
        self.0.lock().unwrap().clear();
    }
    fn set_device_address(&self, _: u8) {}
    fn write(&self, _: EndpointAddress, data: &[u8]) -> usb_device::Result<usize> {
        Ok(data.len())
    }
    fn read(&self, _: EndpointAddress, data: &mut [u8]) -> usb_device::Result<usize> {
        let mut packets = self.0.lock().unwrap();
        let packet = packets.front().ok_or(UsbError::WouldBlock)?;
        if packet.len() > data.len() {
            return Err(UsbError::BufferOverflow);
        }
        data[..packet.len()].copy_from_slice(packet);
        Ok(packets.pop_front().unwrap().len())
    }
    fn set_stalled(&self, _: EndpointAddress, _: bool) {}
    fn is_stalled(&self, _: EndpointAddress) -> bool {
        false
    }
    fn suspend(&self) {}
    fn resume(&self) {}
    fn poll(&self) -> PollResult {
        PollResult::None
    }
}
struct Backlight;
impl embedded_hal::pwm::ErrorType for Backlight {
    type Error = core::convert::Infallible;
}
impl SetDutyCycle for Backlight {
    fn max_duty_cycle(&self) -> u16 {
        100
    }
    fn set_duty_cycle(&mut self, _: u16) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn setup() -> (GudDevice<'static, Bus, Backlight>, Packets) {
    let packets = Packets::default();
    let bus = Box::leak(Box::new(UsbBusAllocator::new(Bus(packets.clone()))));
    let free = Box::leak(Box::new(FreeChannel::new()));
    for _ in 0..NUM_BANDS {
        assert!(free.try_send(Box::leak(Box::new([0; BAND_BYTES]))).is_ok());
    }
    let jobs = Box::leak(Box::new(JobChannel::new()));
    let gud = GudDevice::new(bus, Backlight, jobs, free);
    let _usb = UsbDeviceBuilder::new(bus, UsbVidPid(0x1d50, 0x614d)).build();
    (gud, packets)
}
fn update(gud: &GudDevice<'_, Bus, Backlight>, y: u16, bytes: u32) {
    gud.commands
        .try_send(Command::Update(Rect {
            x: 0,
            y,
            width: (bytes / 2) as u16,
            height: 1,
            length: bytes,
            payload: bytes,
            compressed: false,
        }))
        .unwrap();
}
fn band(gud: &GudDevice<'_, Bus, Backlight>, y: u16, expected: &[u8]) {
    let Ok(PanelJob::Band { payload, rect }) = gud.jobs.try_receive() else {
        panic!("missing band")
    };
    assert_eq!(rect.y, y);
    assert_eq!(&payload[..expected.len()], expected);
    assert!(gud.free.try_send(payload).is_ok());
}

#[test]
fn next_command_preserves_previous_unread_tail() {
    let (mut gud, packets) = setup();
    update(&gud, 0, 66);
    packets.lock().unwrap().push_back(vec![7; 64]);
    gud.service(10);
    // EP0 queued the next SET_BUFFER before class polling read the last packet.
    update(&gud, 1, 4);
    packets
        .lock()
        .unwrap()
        .extend([vec![8, 9], vec![1], vec![2, 3, 4]]);
    gud.service(20);
    let mut first = vec![7; 64];
    first.extend([8, 9]);
    band(&gud, 0, &first);
    band(&gud, 1, &[1, 2, 3, 4]);
    assert_eq!(gud.stats().updates, 2);
    assert_eq!(gud.stats().bytes, 70);
    assert_eq!(gud.free.len(), NUM_BANDS);
}

#[test]
fn exhausted_buffers_backpressure_without_consuming_next_packet() {
    let (mut gud, packets) = setup();
    for y in 0..4 {
        update(&gud, y, 2);
        packets.lock().unwrap().push_back(vec![y as u8; 2]);
    }
    gud.service(1);
    assert_eq!(gud.free.len(), 0);
    assert_eq!(gud.jobs.len(), 3);
    assert_eq!(packets.lock().unwrap().len(), 1);
    // Waiting for a free buffer is not a stalled host payload, regardless of duration.
    gud.service(3_000_000);
    band(&gud, 0, &[0, 0]);
    gud.service(3_000_001);
    for y in 1..4 {
        band(&gud, y, &[y as u8; 2]);
    }
    assert_eq!(gud.free.len(), NUM_BANDS);
}

#[test]
fn reset_returns_partial_buffer_and_keeps_complete_jobs_owned() {
    let (mut gud, packets) = setup();
    update(&gud, 0, 2);
    update(&gud, 1, 4);
    packets.lock().unwrap().extend([vec![1, 2], vec![3]]);
    gud.service(1);
    assert_eq!(gud.free.len(), 1);
    gud.reset();
    assert_eq!(gud.free.len(), 2);
    assert!(gud.receiving.is_none());
    assert!(gud.commands.is_empty());
    band(&gud, 0, &[1, 2]);
    update(&gud, 2, 2);
    packets.lock().unwrap().push_back(vec![9, 8]);
    gud.service(2);
    band(&gud, 2, &[9, 8]);
    assert_eq!(gud.free.len(), NUM_BANDS);
}

#[test]
fn panel_queue_backpressure_preserves_pending_job() {
    let (mut gud, packets) = setup();
    for _ in 0..4 {
        assert!(gud.jobs.try_send(PanelJob::Enable(true)).is_ok());
    }
    update(&gud, 0, 2);
    packets.lock().unwrap().push_back(vec![8, 9]);
    gud.service(1);
    assert!(gud.pending_job.is_some());
    for _ in 0..4 {
        assert!(matches!(gud.jobs.try_receive(), Ok(PanelJob::Enable(true))));
    }
    gud.service(2);
    band(&gud, 0, &[8, 9]);
}

#[test]
#[should_panic(expected = "payload stalled")]
fn payload_overrun_resets_instead_of_truncating() {
    let (mut gud, packets) = setup();
    update(&gud, 0, 2);
    packets.lock().unwrap().push_back(vec![1, 2, 3]);
    gud.service(1);
}

#[test]
#[should_panic(expected = "payload stalled")]
fn partial_payload_timeout_resets() {
    let (mut gud, packets) = setup();
    update(&gud, 0, 4);
    packets.lock().unwrap().push_back(vec![1]);
    gud.service(1);
    gud.service(2_000_001);
}

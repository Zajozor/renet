use std::{
    collections::{HashMap, VecDeque},
    time::Duration,
};

use bytes::Bytes;

use crate::{error::ChannelError, packet::Packet};

use super::{slice_constructor::Slice, SliceConstructor, SLICE_SIZE};

#[derive(Debug)]
pub struct SendChannelUnreliable {
    channel_id: u8,
    unreliable_messages: VecDeque<Bytes>,
    sliced_message_id: u64,
    max_memory_usage_bytes: usize,
    memory_usage_bytes: usize,
    error: Option<ChannelError>,
}

#[derive(Debug)]
pub struct ReceiveChannelUnreliable {
    channel_id: u8,
    messages: VecDeque<Bytes>,
    slices: HashMap<u64, SliceConstructor>,
    slices_last_received: HashMap<u64, Duration>,
    max_memory_usage_bytes: usize,
    memory_usage_bytes: usize,
    error: Option<ChannelError>,
}

impl SendChannelUnreliable {
    pub fn new(channel_id: u8, max_memory_usage_bytes: usize) -> Self {
        Self {
            channel_id,
            unreliable_messages: VecDeque::new(),
            sliced_message_id: 0,
            max_memory_usage_bytes,
            memory_usage_bytes: 0,
            error: None,
        }
    }

    pub fn get_messages_to_send(&mut self) -> Vec<Packet> {
        let mut packets: Vec<Packet> = vec![];
        let mut small_messages: Vec<Bytes> = vec![];
        let mut small_messages_bytes = 0;

        while let Some(message) = self.unreliable_messages.pop_front() {
            if message.len() > SLICE_SIZE {
                let num_slices = (message.len() + SLICE_SIZE - 1) / SLICE_SIZE;

                for slice_index in 0..num_slices {
                    let start = slice_index * SLICE_SIZE;
                    let end = if slice_index == num_slices - 1 { message.len() } else { (slice_index + 1) * SLICE_SIZE };
                    let payload = message.slice(start..end);

                    let slice = Slice {
                        message_id: self.sliced_message_id,
                        slice_index,
                        num_slices,
                        payload,
                    };

                    packets.push(Packet::UnreliableSlice {
                        channel_id: self.channel_id,
                        slice,
                    });
                }

                self.sliced_message_id += 1;
            } else {
                if small_messages_bytes + message.len() > SLICE_SIZE {
                    packets.push(Packet::SmallUnreliable {
                        channel_id: self.channel_id,
                        messages: std::mem::take(&mut small_messages),
                    });
                    small_messages_bytes = 0;
                }

                small_messages_bytes += message.len();
                small_messages.push(message);
            }
        }

        // Generate final packet for remaining small messages
        if !small_messages.is_empty() {
            packets.push(Packet::SmallUnreliable {
                channel_id: self.channel_id,
                messages: std::mem::take(&mut small_messages),
            });
        }

        packets
    }

    pub fn send_message(&mut self, message: Bytes) {
        if self.max_memory_usage_bytes < self.memory_usage_bytes + message.len() {
            // TODO: log::warm
            return;
        }

        self.memory_usage_bytes += message.len();
        self.unreliable_messages.push_back(message);
    }
}

impl ReceiveChannelUnreliable {
    pub fn new(channel_id: u8, max_memory_usage_bytes: usize) -> Self {
        Self {
            channel_id,
            slices: HashMap::new(),
            slices_last_received: HashMap::new(),
            messages: VecDeque::new(),
            memory_usage_bytes: 0,
            max_memory_usage_bytes,
            error: None,
        }
    }

    pub fn process_message(&mut self, message: Bytes) {
        if self.max_memory_usage_bytes < self.memory_usage_bytes + message.len() {
            // FIXME: log::warn dropped message
            return;
        }

        self.memory_usage_bytes += message.len();
        self.messages.push_back(message.into());
    }

    pub fn process_slice(&mut self, slice: Slice, current_time: Duration) {
        if !self.slices.contains_key(&slice.message_id) {
            let message_len = slice.num_slices * SLICE_SIZE;
            if self.max_memory_usage_bytes < self.memory_usage_bytes + message_len {
                // FIXME: log::warn dropped message
                return;
            }

            self.memory_usage_bytes += message_len;
        }

        let slice_constructor = self
            .slices
            .entry(slice.message_id)
            .or_insert_with(|| SliceConstructor::new(slice.message_id, slice.num_slices));

        match slice_constructor.process_slice(slice.slice_index, &slice.payload) {
            Err(e) => self.error = Some(e),
            Ok(Some(message)) => {
                self.slices.remove(&slice.message_id);
                self.slices_last_received.remove(&slice.message_id);
                self.memory_usage_bytes -= slice.num_slices * SLICE_SIZE;
                self.memory_usage_bytes += message.len();
                self.messages.push_back(message);
            }
            Ok(None) => {
                self.slices_last_received.insert(slice.message_id, current_time);
            }
        }
    }

    pub fn receive_message(&mut self) -> Option<Bytes> {
        let Some(message) = self.messages.pop_front() else {
            return None
        };
        self.memory_usage_bytes -= message.len();
        Some(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_packet() {
        let max_memory: usize = 10000;
        let mut recv = ReceiveChannelUnreliable::new(0, max_memory);
        let mut send = SendChannelUnreliable::new(0, max_memory);

        let message1 = vec![1, 2, 3];
        let message2 = vec![3, 4, 5];

        send.send_message(message1.clone().into());
        send.send_message(message2.clone().into());

        let packets = send.get_messages_to_send();
        for packet in packets {
            let Packet::SmallUnreliable { channel_id: 0, messages } = packet else {
                unreachable!();
            };
            for message in messages {
                recv.process_message(message);
            }
        }

        let new_message1 = recv.receive_message().unwrap();
        let new_message2 = recv.receive_message().unwrap();
        assert!(recv.receive_message().is_none());

        assert_eq!(message1, new_message1);
        assert_eq!(message2, new_message2);

        let packets = send.get_messages_to_send();
        assert!(packets.is_empty());
    }

    #[test]
    fn slice_packet() {
        let max_memory: usize = 10000;
        let current_time = Duration::ZERO;
        let mut recv = ReceiveChannelUnreliable::new(0, max_memory);
        let mut send = SendChannelUnreliable::new(0, max_memory);

        let message = vec![5; SLICE_SIZE * 3];

        send.send_message(message.clone().into());

        let packets = send.get_messages_to_send();
        for packet in packets {
            let Packet::UnreliableSlice { channel_id: 0, slice } = packet else {
                unreachable!();
            };
            recv.process_slice(slice, current_time);
        }

        let new_message = recv.receive_message().unwrap();
        assert!(recv.receive_message().is_none());

        assert_eq!(message, new_message);

        let packets = send.get_messages_to_send();
        assert!(packets.is_empty());
    }

    #[test]
    fn max_memory() {
        let mut recv = ReceiveChannelUnreliable::new(0, 50);
        let mut send = SendChannelUnreliable::new(0, 40);

        let message = vec![5; 50];

        send.send_message(message.clone().into());
        send.send_message(message.clone().into());

        let packets = send.get_messages_to_send();
        for packet in packets {
            let Packet::SmallUnreliable { channel_id: 0, messages } = packet else {
                unreachable!();
            };

            // Second message was dropped
            assert_eq!(messages.len(), 1);
            for message in messages {
                recv.process_message(message);
            }
        }

        // The processed message was dropped because there was no memory available
        assert!(recv.receive_message().is_none());
    }
}
/// Implements telemetry sending functionality.
///
/// Sends only basic anonymous usage statistics like program version, used commands and brokers.
/// No personal information will ever be sent.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Instant, Duration};

use diesel::{self, prelude::*};
use log::{trace, error};
use reqwest::blocking::Client;
use serde::Serialize;
use serde_json::Value;

use crate::brokers::Broker;
use crate::core::{EmptyResult, GenericResult};
use crate::db::{self, schema::telemetry, models};

// FIXME(konishchev): Add more fields
#[derive(Serialize, Clone)]
pub struct TelemetryRecord {
    command: String,
    brokers: Vec<String>,
}

impl TelemetryRecord {
    // FIXME(konishchev): Rewrite
    #[cfg(test)]
    fn mock(id: usize) -> TelemetryRecord {
        TelemetryRecord {
            command: format!("{}", id),
            brokers: Vec::new(),
        }
    }
}

pub struct TelemetryRecordBuilder {
    brokers: HashSet<Broker>,
}

impl TelemetryRecordBuilder {
    pub fn new() -> TelemetryRecordBuilder {
        TelemetryRecordBuilder {
            brokers: HashSet::new(),
        }
    }

    pub fn new_with_broker(broker: Broker) -> TelemetryRecordBuilder {
        let mut record = TelemetryRecordBuilder::new();
        record.add_broker(broker);
        record
    }

    pub fn add_broker(&mut self, broker: Broker) {
        self.brokers.insert(broker);
    }

    pub fn build(self, command: &str) -> TelemetryRecord {
        let mut brokers: Vec<String> = self.brokers.iter()
            .map(|broker| broker.id().to_owned()).collect();
        brokers.sort();

        TelemetryRecord {
            command: command.to_owned(),
            brokers,
        }
    }
}

#[derive(Serialize)]
struct TelemetryRequest {
    records: Vec<Value>,
}

// FIXME(konishchev): Configuration option
pub struct Telemetry {
    db: db::Connection,
    sender: Option<(JoinHandle<Option<i64>>, Instant)>,
}

impl Telemetry {
    pub fn new(
        connection: db::Connection,
        flush_threshold: usize, flush_timeout: Duration, max_records: usize,
    ) -> GenericResult<Telemetry> {
        let mut telemetry = Telemetry {
            db: connection,
            sender: None,
        };

        telemetry.sender = telemetry.load(max_records)?.map(|(records, last_record_id)| {
            // By default we don't give any extra time to sender to complete its work. But if we
            // accumulated some records - we do.
            let mut deadline = Instant::now();
            if records.len() % flush_threshold == 0 {
                deadline += flush_timeout;
            }

            let request = TelemetryRequest {records};
            let sender = thread::spawn(move || send(request, last_record_id));
            (sender, deadline)
        });

        Ok(telemetry)
    }

    pub fn add(&self, record: TelemetryRecord) -> EmptyResult {
        let payload = serde_json::to_string(&record)?;

        diesel::insert_into(telemetry::table)
            .values(models::NewTelemetryRecord {payload})
            .execute(&*self.db)?;

        Ok(())
    }

    fn load(&self, max_records: usize) -> GenericResult<Option<(Vec<Value>, i64)>> {
        let records = telemetry::table
            .select((telemetry::id, telemetry::payload))
            .order_by(telemetry::id.asc())
            .load::<(i64, String)>(&*self.db)?;

        let mut records: &[_] = &records;
        if records.len() > max_records {
            let count = records.len() - max_records;
            trace!("Dropping {} telemetry records.", count);
            self.delete(records[count - 1].0)?;
            records = &records[count..];
        }

        let mut payloads = Vec::with_capacity(records.len());
        for record in records {
            let payload = serde_json::from_str(&record.1).map_err(|e| format!(
                "Failed to parse telemetry record: {}", e))?;
            payloads.push(payload);
        }

        Ok(records.last().map(|record| (payloads, record.0)))
    }

    fn delete(&self, last_record_id: i64) -> EmptyResult {
        diesel::delete(telemetry::table.filter(telemetry::id.le(last_record_id)))
            .execute(&*self.db)?;
        Ok(())
    }

    #[cfg(test)]
    fn close(mut self) -> EmptyResult {
        self.close_impl()
    }

    fn close_impl(&mut self) -> EmptyResult {
        if let Some(last_record_id) = self.wait_sender() {
            self.delete(last_record_id).map_err(|e| format!(
                "Failed to delete telemetry records: {}", e))?;
        }
        Ok(())
    }

    fn wait_sender(&mut self) -> Option<i64> {
        let (sender, deadline) = match self.sender.take() {
            Some(value) => value,
            None => return None,
        };

        let result = Arc::new(Mutex::new(None));
        let joiner = {
            // We use additional thread to be able to join with timeout

            let result = result.clone();
            let waiter = thread::current();

            thread::spawn(move || {
                let value = sender.join().unwrap();
                result.lock().unwrap().replace(value);
                waiter.unpark();
            })
        };

        while let Some(timeout) = deadline.checked_duration_since(Instant::now()) {
            if result.lock().unwrap().is_some() {
                break;
            }
            thread::park_timeout(timeout);
        }
        let result = result.lock().unwrap().take();

        if cfg!(test) {
            // Join the thread in test mode to not introduce any side effects, but after result
            // acquiring.
            joiner.join().unwrap();
        } else {
            // We mustn't delay program execution or shutdown because of telemetry server or network
            // unavailability, so just forget about the thread - it will die on program exit.
        }

        result.unwrap_or(None)
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        if let Err(err) = self.close_impl() {
            error!("{}.", err)
        }
    }
}

fn send(request: TelemetryRequest, last_record_id: i64) -> Option<i64> {
    #[cfg(not(test))] let base_url = "https://investments.konishchev.ru";
    #[cfg(test)] let base_url = mockito::server_url();
    let url = format!("{}/telemetry", base_url);

    trace!("Sending telemetry ({} records)...", request.records.len());
    match Client::new().post(url).json(&request).send() {
        Ok(response) => {
            let status = response.status();
            if status.is_success() {
                // Consume body in test mode to block on unreachable server emulation
                if cfg!(test) {
                    let _ = response.bytes();
                }
                trace!("Telemetry has been successfully sent.");
                Some(last_record_id)
            } else {
                trace!("Telemetry server returned an error: {}.", status);
                None
            }
        },
        Err(e) => {
            trace!("Failed to send telemetry: {}.", e);
            None
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{self, Mock, mock};

    #[test]
    fn telemetry() {
        let (_database, connection) = db::new_temporary();
        let new_telemetry = || {
            let flush_threshold = 1;
            let flush_timeout = Duration::from_millis(10);
            let max_records = 5;
            Telemetry::new(connection.clone(), flush_threshold, flush_timeout, max_records).unwrap()
        };

        let mut expected = vec![];
        let mut server = broken_server().expect(0);

        // Broken server, nothing to drop, nothing to send
        {
            let telemetry = new_telemetry();

            for id in 0..4 {
                let record = TelemetryRecord::mock(id);
                telemetry.add(record.clone()).unwrap();
                expected.push(record);
            }

            telemetry.close().unwrap();
        }
        server.assert();
        compare(connection.clone(), &expected); // 4 records

        // Broken server, nothing to drop, trying to send
        {
            let telemetry = new_telemetry();

            for id in 4..8 {
                let record = TelemetryRecord::mock(id);
                telemetry.add(record.clone()).unwrap();
                expected.push(record);
            }

            telemetry.close().unwrap();
        }
        server = server.expect(1);
        server.assert();
        compare(connection.clone(), &expected); // 8 records

        // Broken server, dropping records, trying to send
        {
            let telemetry = new_telemetry();
            expected.drain(..3);

            for id in 8..12 {
                let record = TelemetryRecord::mock(id);
                telemetry.add(record.clone()).unwrap();
                expected.push(record);
            }

            telemetry.close().unwrap();
        }
        server = server.expect(2);
        server.assert();
        compare(connection.clone(), &expected); // 9 records

        // Healthy server, dropping records, sending remaining
        expected.drain(..4);
        server = healthy_server(&expected); // 5 records
        {
            let telemetry = new_telemetry();

            for id in 12..16 {
                let record = TelemetryRecord::mock(id);
                telemetry.add(record.clone()).unwrap();
                expected.push(record);
            }

            telemetry.close().unwrap();
        }
        server.assert();
        expected.drain(..5);
        compare(connection.clone(), &expected); // 4 records

        // Unreachable server, nothing to drop, trying to send
        server = unreachable_server();
        {
            let telemetry = new_telemetry();

            let record = TelemetryRecord::mock(16);
            telemetry.add(record.clone()).unwrap();
            expected.push(record);

            telemetry.close().unwrap();
        }
        server.assert();
        compare(connection.clone(), &expected); // 5 records

        // Healthy server, nothing to drop, sending all records
        server = healthy_server(&expected);
        {
            let telemetry = new_telemetry();

            let record = TelemetryRecord::mock(17);
            telemetry.add(record.clone()).unwrap();
            expected.push(record);

            telemetry.close().unwrap();
        }
        server.assert();
        expected.drain(..5);
        compare(connection.clone(), &expected); // 1 record
    }

    fn broken_server() -> Mock {
        mock("POST", "/telemetry")
            .with_status(500)
            .create()
    }

    fn healthy_server(expected: &[TelemetryRecord]) -> Mock {
        let expected_request = TelemetryRequest {
            records: expected.iter().map(|record| {
                serde_json::to_value(record).unwrap()
            }).collect(),
        };
        let expected_body = serde_json::to_string(&expected_request).unwrap();

        mock("POST", "/telemetry")
            .match_header("content-type", "application/json")
            .match_body(expected_body.as_str())
            .with_status(200)
            .create()
    }

    fn unreachable_server() -> Mock {
        mock("POST", "/telemetry")
            .with_status(200)
            .with_body_from_fn(|_| {
                thread::sleep(Duration::from_millis(100));
                Ok(())
            })
            .create()
    }

    fn compare(connection: db::Connection, expected: &[TelemetryRecord]) {
        let actual = telemetry::table
            .select(telemetry::payload)
            .order_by(telemetry::id.asc())
            .load::<String>(&*connection).unwrap();

        let expected: Vec<String> = expected.iter()
            .map(|record| serde_json::to_string(record).unwrap())
            .collect();

        assert_eq!(actual, expected);
    }
}
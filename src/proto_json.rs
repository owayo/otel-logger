//! JSONL へ書く JSON を、非有限の double まで含めて保存する serializer。
//!
//! opentelemetry-proto 0.33.1 の `with-serde` は、特殊値 (NaN / ±Infinity) を文字列で書く
//! serializer を `ValueAtQuantile` にしか付けていない。`AnyValue::DoubleValue`・
//! `NumberDataPoint::AsDouble`・histogram の `sum` / `min` / `max` などは素の `f64` のままで、
//! serde_json は非有限値を `null` で書く。そのままでは受信した値が JSONL から消え、
//! exporter には 200 を返してしまう。
//!
//! ここでは serde の `Serializer` を包み、非有限の `f64` / `f32` だけを proto3 JSON mapping
//! と同じ文字列 (`"NaN"` / `"Infinity"` / `"-Infinity"`) にする。有限値の出力は
//! `serde_json::to_vec` とバイト単位で一致する。読み戻す側 (opentelemetry-proto の
//! `Deserialize`) はまだ文字列を受け付けないので、replay では該当 batch が decode 失敗として
//! 表に出る。上流の open-telemetry/opentelemetry-rust #3442 / PR #3496 が入れば読める。

use serde::ser::{self, Serialize, Serializer};

/// `value` を JSON にする。非有限の double は proto3 JSON mapping の文字列で書く。
pub fn to_vec<T: ?Sized + Serialize>(value: &T) -> serde_json::Result<Vec<u8>> {
    serde_json::to_vec(&ProtoJson(value))
}

/// 中身を `NonFinite` で包んだ serializer へ渡す。入れ子の値も同じ扱いにするため、
/// 子の値は必ずこれで包んでから委譲する。
struct ProtoJson<'a, T: ?Sized>(&'a T);

impl<T: ?Sized + Serialize> Serialize for ProtoJson<'_, T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(NonFinite(serializer))
    }
}

/// 非有限の浮動小数点数だけを書き換え、それ以外は内側の serializer へそのまま委譲する。
struct NonFinite<S>(S);

macro_rules! forward_scalars {
    ($($name:ident($ty:ty)),* $(,)?) => {
        $(fn $name(self, v: $ty) -> Result<S::Ok, S::Error> { self.0.$name(v) })*
    };
}

impl<S: Serializer> Serializer for NonFinite<S> {
    type Ok = S::Ok;
    type Error = S::Error;
    type SerializeSeq = NonFinite<S::SerializeSeq>;
    type SerializeTuple = NonFinite<S::SerializeTuple>;
    type SerializeTupleStruct = NonFinite<S::SerializeTupleStruct>;
    type SerializeTupleVariant = NonFinite<S::SerializeTupleVariant>;
    type SerializeMap = NonFinite<S::SerializeMap>;
    type SerializeStruct = NonFinite<S::SerializeStruct>;
    type SerializeStructVariant = NonFinite<S::SerializeStructVariant>;

    fn serialize_f64(self, v: f64) -> Result<S::Ok, S::Error> {
        if v.is_nan() {
            self.0.serialize_str("NaN")
        } else if v.is_infinite() {
            self.0
                .serialize_str(if v > 0.0 { "Infinity" } else { "-Infinity" })
        } else {
            self.0.serialize_f64(v)
        }
    }

    fn serialize_f32(self, v: f32) -> Result<S::Ok, S::Error> {
        if v.is_finite() {
            self.0.serialize_f32(v)
        } else {
            self.serialize_f64(f64::from(v))
        }
    }

    forward_scalars!(
        serialize_bool(bool),
        serialize_i8(i8),
        serialize_i16(i16),
        serialize_i32(i32),
        serialize_i64(i64),
        serialize_i128(i128),
        serialize_u8(u8),
        serialize_u16(u16),
        serialize_u32(u32),
        serialize_u64(u64),
        serialize_u128(u128),
        serialize_char(char),
        serialize_str(&str),
        serialize_bytes(&[u8]),
        serialize_unit_struct(&'static str),
    );

    fn serialize_none(self) -> Result<S::Ok, S::Error> {
        self.0.serialize_none()
    }

    fn serialize_unit(self) -> Result<S::Ok, S::Error> {
        self.0.serialize_unit()
    }

    fn serialize_some<T: ?Sized + Serialize>(self, v: &T) -> Result<S::Ok, S::Error> {
        self.0.serialize_some(&ProtoJson(v))
    }

    fn serialize_unit_variant(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
    ) -> Result<S::Ok, S::Error> {
        self.0.serialize_unit_variant(name, index, variant)
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        name: &'static str,
        v: &T,
    ) -> Result<S::Ok, S::Error> {
        self.0.serialize_newtype_struct(name, &ProtoJson(v))
    }

    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        v: &T,
    ) -> Result<S::Ok, S::Error> {
        self.0
            .serialize_newtype_variant(name, index, variant, &ProtoJson(v))
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<Self::SerializeSeq, S::Error> {
        self.0.serialize_seq(len).map(NonFinite)
    }

    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, S::Error> {
        self.0.serialize_tuple(len).map(NonFinite)
    }

    fn serialize_tuple_struct(
        self,
        name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleStruct, S::Error> {
        self.0.serialize_tuple_struct(name, len).map(NonFinite)
    }

    fn serialize_tuple_variant(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleVariant, S::Error> {
        self.0
            .serialize_tuple_variant(name, index, variant, len)
            .map(NonFinite)
    }

    fn serialize_map(self, len: Option<usize>) -> Result<Self::SerializeMap, S::Error> {
        self.0.serialize_map(len).map(NonFinite)
    }

    fn serialize_struct(
        self,
        name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStruct, S::Error> {
        self.0.serialize_struct(name, len).map(NonFinite)
    }

    fn serialize_struct_variant(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStructVariant, S::Error> {
        self.0
            .serialize_struct_variant(name, index, variant, len)
            .map(NonFinite)
    }

    fn is_human_readable(&self) -> bool {
        self.0.is_human_readable()
    }
}

macro_rules! compound_elements {
    ($trait_name:ident, $method:ident) => {
        impl<S: ser::$trait_name> ser::$trait_name for NonFinite<S> {
            type Ok = S::Ok;
            type Error = S::Error;

            fn $method<T: ?Sized + Serialize>(&mut self, v: &T) -> Result<(), S::Error> {
                self.0.$method(&ProtoJson(v))
            }

            fn end(self) -> Result<S::Ok, S::Error> {
                self.0.end()
            }
        }
    };
}

compound_elements!(SerializeSeq, serialize_element);
compound_elements!(SerializeTuple, serialize_element);
compound_elements!(SerializeTupleStruct, serialize_field);
compound_elements!(SerializeTupleVariant, serialize_field);

// `#[serde(flatten)]` の値も、外側の struct を map として書く FlatMapSerializer を通って
// ここへ来るので、flatten された oneof (`asDouble` / `doubleValue`) にも効く。
impl<S: ser::SerializeMap> ser::SerializeMap for NonFinite<S> {
    type Ok = S::Ok;
    type Error = S::Error;

    fn serialize_key<T: ?Sized + Serialize>(&mut self, k: &T) -> Result<(), S::Error> {
        self.0.serialize_key(&ProtoJson(k))
    }

    fn serialize_value<T: ?Sized + Serialize>(&mut self, v: &T) -> Result<(), S::Error> {
        self.0.serialize_value(&ProtoJson(v))
    }

    fn end(self) -> Result<S::Ok, S::Error> {
        self.0.end()
    }
}

macro_rules! compound_fields {
    ($trait_name:ident) => {
        impl<S: ser::$trait_name> ser::$trait_name for NonFinite<S> {
            type Ok = S::Ok;
            type Error = S::Error;

            fn serialize_field<T: ?Sized + Serialize>(
                &mut self,
                key: &'static str,
                v: &T,
            ) -> Result<(), S::Error> {
                self.0.serialize_field(key, &ProtoJson(v))
            }

            fn skip_field(&mut self, key: &'static str) -> Result<(), S::Error> {
                self.0.skip_field(key)
            }

            fn end(self) -> Result<S::Ok, S::Error> {
                self.0.end()
            }
        }
    };
}

compound_fields!(SerializeStruct);
compound_fields!(SerializeStructVariant);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::TelemetryRecord;
    use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
    use opentelemetry_proto::tonic::common::v1::any_value::Value as OtlpValue;
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
    use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value as NumberValue;
    use opentelemetry_proto::tonic::metrics::v1::{
        Gauge, Histogram, HistogramDataPoint, Metric, NumberDataPoint, ResourceMetrics,
        ScopeMetrics, metric::Data,
    };

    /// 同じ値を gauge の点・属性の double・histogram の sum / min / 境界に入れた metrics batch。
    fn metrics_with(values: &[f64]) -> TelemetryRecord {
        let point = |v: f64| NumberDataPoint {
            attributes: vec![KeyValue {
                key: "attr".into(),
                value: Some(AnyValue {
                    value: Some(OtlpValue::DoubleValue(v)),
                }),
                key_strindex: 0,
            }],
            time_unix_nano: 1_760_000_000_123_456_789,
            value: Some(NumberValue::AsDouble(v)),
            ..Default::default()
        };
        let histogram_point = |v: f64| HistogramDataPoint {
            sum: Some(v),
            min: Some(v),
            count: 3,
            bucket_counts: vec![1, 2],
            explicit_bounds: vec![v],
            ..Default::default()
        };
        TelemetryRecord::Metrics(Box::new(ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                scope_metrics: vec![ScopeMetrics {
                    metrics: vec![
                        Metric {
                            name: "g".into(),
                            data: Some(Data::Gauge(Gauge {
                                data_points: values.iter().copied().map(point).collect(),
                            })),
                            ..Default::default()
                        },
                        Metric {
                            name: "h".into(),
                            data: Some(Data::Histogram(Histogram {
                                data_points: values.iter().copied().map(histogram_point).collect(),
                                aggregation_temporality: 1,
                            })),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }))
    }

    /// 有限値だけなら、従来の `serde_json::to_vec` と 1 byte も変えない
    /// (既存の JSONL と、それを読む replay / 外部の解析を壊さない)。
    #[test]
    fn finite_values_are_written_exactly_as_before() {
        let record = metrics_with(&[1.5, -0.0, 1e300, 0.1, 3.0, -2.25]);
        assert_eq!(
            to_vec(&record).unwrap(),
            serde_json::to_vec(&record).unwrap()
        );
    }

    /// 回帰テスト: 非有限の double を `null` にせず、proto3 JSON mapping の文字列で残す。
    ///
    /// serde_json は NaN / ±Infinity を `null` で書くため、受信した値が JSONL から消えたまま
    /// exporter に 200 を返していた。flatten された oneof (`asDouble` / `doubleValue`) も対象。
    #[test]
    fn non_finite_doubles_are_kept_as_proto_json_strings() {
        let values = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1.5];
        let written: serde_json::Value =
            serde_json::from_slice(&to_vec(&metrics_with(&values)).unwrap()).unwrap();
        let expected = ["NaN", "Infinity", "-Infinity"]
            .map(serde_json::Value::from)
            .into_iter()
            .chain([serde_json::Value::from(1.5)])
            .collect::<Vec<_>>();
        let metrics = &written["resourceMetrics"][0]["scopeMetrics"][0]["metrics"];
        let gauge = metrics[0]["gauge"]["dataPoints"].as_array().unwrap();
        let histogram = metrics[1]["histogram"]["dataPoints"].as_array().unwrap();
        for (i, want) in expected.iter().enumerate() {
            assert_eq!(&gauge[i]["asDouble"], want, "gauge[{i}]: {written}");
            assert_eq!(
                &gauge[i]["attributes"][0]["value"]["doubleValue"], want,
                "attribute[{i}]: {written}"
            );
            assert_eq!(&histogram[i]["sum"], want, "sum[{i}]: {written}");
            assert_eq!(&histogram[i]["min"], want, "min[{i}]: {written}");
            assert_eq!(
                &histogram[i]["explicitBounds"][0], want,
                "bounds[{i}]: {written}"
            );
        }
        // 値の無い optional は従来どおり null のまま (NaN と区別できる)。
        assert!(histogram[0]["max"].is_null());
    }

    use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
    use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
    use opentelemetry_proto::tonic::common::v1::{ArrayValue, InstrumentationScope, KeyValueList};
    use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
    use opentelemetry_proto::tonic::metrics::v1::exponential_histogram_data_point::Buckets;
    use opentelemetry_proto::tonic::metrics::v1::summary_data_point::ValueAtQuantile;
    use opentelemetry_proto::tonic::metrics::v1::{
        Exemplar, ExponentialHistogram, ExponentialHistogramDataPoint, Sum, Summary,
        SummaryDataPoint, exemplar,
    };
    use opentelemetry_proto::tonic::resource::v1::Resource;
    use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span, Status, span};
    use serde::Serialize;
    use std::collections::BTreeMap;

    fn kv(key: &str, value: OtlpValue) -> KeyValue {
        KeyValue {
            key: key.into(),
            value: Some(AnyValue { value: Some(value) }),
            key_strindex: 0,
        }
    }

    /// AnyValue の全種類を並べ、double は配列・kvlist の入れ子の奥にも入れた属性。
    fn any_values_with(v: f64) -> Vec<KeyValue> {
        let double = || OtlpValue::DoubleValue(v);
        vec![
            kv("s", OtlpValue::StringValue("text".into())),
            kv("b", OtlpValue::BoolValue(true)),
            kv("i", OtlpValue::IntValue(-7)),
            kv("d", double()),
            kv("bytes", OtlpValue::BytesValue(vec![0, 255])),
            kv(
                "arr",
                OtlpValue::ArrayValue(ArrayValue {
                    values: vec![
                        AnyValue {
                            value: Some(double()),
                        },
                        AnyValue {
                            value: Some(OtlpValue::ArrayValue(ArrayValue {
                                values: vec![AnyValue {
                                    value: Some(double()),
                                }],
                            })),
                        },
                    ],
                }),
            ),
            kv(
                "kv",
                OtlpValue::KvlistValue(KeyValueList {
                    values: vec![
                        kv("inner", double()),
                        kv(
                            "deeper",
                            OtlpValue::KvlistValue(KeyValueList {
                                values: vec![kv("leaf", double())],
                            }),
                        ),
                    ],
                }),
            ),
        ]
    }

    fn resource() -> Option<Resource> {
        Some(Resource {
            attributes: vec![kv("service.name", OtlpValue::StringValue("svc".into()))],
            ..Default::default()
        })
    }

    fn logs_with(v: f64) -> TelemetryRecord {
        TelemetryRecord::Logs(Box::new(ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: resource(),
                scope_logs: vec![ScopeLogs {
                    scope: Some(InstrumentationScope {
                        name: "scope".into(),
                        attributes: any_values_with(v),
                        ..Default::default()
                    }),
                    log_records: vec![LogRecord {
                        time_unix_nano: 1_760_000_000_123_456_789,
                        observed_time_unix_nano: 1_760_000_000_223_456_789,
                        severity_number: 9,
                        severity_text: "INFO".into(),
                        body: Some(AnyValue {
                            value: Some(OtlpValue::KvlistValue(KeyValueList {
                                values: any_values_with(v),
                            })),
                        }),
                        attributes: any_values_with(v),
                        flags: 1,
                        trace_id: vec![1; 16],
                        span_id: vec![2; 8],
                        event_name: "event".into(),
                        ..Default::default()
                    }],
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        }))
    }

    fn traces_with(v: f64) -> TelemetryRecord {
        TelemetryRecord::Traces(Box::new(ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: resource(),
                scope_spans: vec![ScopeSpans {
                    spans: vec![Span {
                        trace_id: vec![1; 16],
                        span_id: vec![2; 8],
                        parent_span_id: vec![3; 8],
                        name: "span".into(),
                        kind: 2,
                        start_time_unix_nano: 1_760_000_000_000_000_000,
                        end_time_unix_nano: 1_760_000_001_000_000_000,
                        attributes: any_values_with(v),
                        events: vec![span::Event {
                            time_unix_nano: 1_760_000_000_500_000_000,
                            name: "event".into(),
                            attributes: any_values_with(v),
                            dropped_attributes_count: 0,
                        }],
                        links: vec![span::Link {
                            trace_id: vec![4; 16],
                            span_id: vec![5; 8],
                            attributes: any_values_with(v),
                            ..Default::default()
                        }],
                        status: Some(Status {
                            message: "boom".into(),
                            code: 2,
                        }),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }))
    }

    /// `metrics_with` に無い種類 (sum の整数点、summary、exponential histogram、exemplar)。
    fn other_metrics_with(v: f64) -> TelemetryRecord {
        TelemetryRecord::Metrics(Box::new(ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                resource: resource(),
                scope_metrics: vec![ScopeMetrics {
                    metrics: vec![
                        Metric {
                            name: "sum".into(),
                            data: Some(Data::Sum(Sum {
                                data_points: vec![NumberDataPoint {
                                    value: Some(NumberValue::AsInt(-3)),
                                    exemplars: vec![Exemplar {
                                        value: Some(exemplar::Value::AsDouble(v)),
                                        time_unix_nano: 1_760_000_000_000_000_001,
                                        span_id: vec![2; 8],
                                        trace_id: vec![1; 16],
                                        filtered_attributes: any_values_with(v),
                                    }],
                                    ..Default::default()
                                }],
                                aggregation_temporality: 1,
                                is_monotonic: true,
                            })),
                            ..Default::default()
                        },
                        Metric {
                            name: "summary".into(),
                            data: Some(Data::Summary(Summary {
                                data_points: vec![SummaryDataPoint {
                                    count: 2,
                                    sum: v,
                                    quantile_values: vec![ValueAtQuantile {
                                        quantile: 0.5,
                                        value: v,
                                    }],
                                    ..Default::default()
                                }],
                            })),
                            ..Default::default()
                        },
                        Metric {
                            name: "exponential".into(),
                            data: Some(Data::ExponentialHistogram(ExponentialHistogram {
                                data_points: vec![ExponentialHistogramDataPoint {
                                    count: 1,
                                    sum: Some(v),
                                    min: Some(v),
                                    max: None,
                                    scale: 1,
                                    zero_threshold: v,
                                    positive: Some(Buckets {
                                        offset: -1,
                                        bucket_counts: vec![1],
                                    }),
                                    ..Default::default()
                                }],
                                aggregation_temporality: 1,
                            })),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }))
    }

    /// JSON 中の数値 `from` をすべて文字列 `to` に置き換え、置き換えた個数を返す。
    fn replace_number(value: &mut serde_json::Value, from: f64, to: &str) -> usize {
        match value {
            serde_json::Value::Number(n) if n.as_f64() == Some(from) => {
                *value = serde_json::Value::from(to);
                1
            }
            serde_json::Value::Array(items) => items
                .iter_mut()
                .map(|item| replace_number(item, from, to))
                .sum(),
            serde_json::Value::Object(fields) => fields
                .values_mut()
                .map(|field| replace_number(field, from, to))
                .sum(),
            _ => 0,
        }
    }

    /// logs / traces / metrics のどれでも、有限値だけなら `serde_json::to_vec` と 1 byte も
    /// 変えない。0 の符号・最小の非正規化数・最大値のような境界の値も同じ書き方にする。
    #[test]
    fn finite_values_of_every_signal_are_written_exactly_as_before() {
        for v in [
            0.0,
            -0.0,
            0.1,
            1.5,
            f64::MIN_POSITIVE,
            5e-324,
            f64::MAX,
            f64::MIN,
        ] {
            for record in [
                logs_with(v),
                traces_with(v),
                other_metrics_with(v),
                metrics_with(&[v]),
            ] {
                assert_eq!(
                    to_vec(&record).unwrap(),
                    serde_json::to_vec(&record).unwrap(),
                    "{} の {v:e}",
                    record.kind()
                );
            }
        }
    }

    /// 回帰テスト: 入れ子の配列・kvlist の奥、span の event / link、exemplar、summary、
    /// exponential histogram にある非有限値も、すべて proto3 JSON mapping の文字列で残す。
    ///
    /// 有限の目印 (1.5) で書いた JSON の数値を文字列へ置き換えたものと一致させることで、
    /// double の位置を 1 つも取りこぼさず、それ以外の部分 (null や他の値) も変えないことを
    /// 確かめる。summary の分位点は opentelemetry-proto 自身が文字列にするので、二重に
    /// 変換しないことも同時に確かめる。
    #[test]
    fn non_finite_doubles_deep_in_every_signal_become_strings() {
        /// 同じ double を全部の位置に入れた record を作る関数。
        type Builder = fn(f64) -> TelemetryRecord;
        const MARKER: f64 = 1.5;
        // 属性 1 組に double が 5 個 (直下・配列の中・配列の中の配列・kvlist・kvlist の中の kvlist)。
        let builders: [(Builder, usize); 3] = [
            // scope の属性・body・record の属性。
            (logs_with, 15),
            // span・event・link の属性。
            (traces_with, 15),
            // exemplar の値と属性、summary の sum と分位点、exponential histogram の 3 項目。
            (other_metrics_with, 11),
        ];
        for (special, name) in [
            (f64::NAN, "NaN"),
            // 符号付きの NaN も、proto3 JSON mapping に無い "-NaN" にはしない。
            (-f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
        ] {
            for (with, at_least) in builders {
                let mut expected: serde_json::Value =
                    serde_json::from_slice(&serde_json::to_vec(&with(MARKER)).unwrap()).unwrap();
                let replaced = replace_number(&mut expected, MARKER, name);
                assert!(replaced >= at_least, "目印は {replaced} 箇所");
                let record = with(special);
                let written: serde_json::Value =
                    serde_json::from_slice(&to_vec(&record).unwrap()).unwrap();
                assert_eq!(written, expected, "{} の {name}", record.kind());
            }
        }
    }

    #[derive(Serialize)]
    struct Newtype(f64);

    #[derive(Serialize)]
    struct Pair(f64, f32);

    #[derive(Serialize)]
    enum Variant {
        Newtype(f64),
        Tuple(f64, f32),
        Struct { value: f64 },
        Unit,
    }

    /// serde のあらゆる形 (f32、Option、入れ子の Vec / map、tuple、newtype、enum の各
    /// variant、flatten) を通る double を持つ型。opentelemetry-proto の型が将来変わっても、
    /// どの形で届いた非有限値も文字列にすることを固定する。
    #[derive(Serialize)]
    struct Shapes {
        single: f32,
        optional_none: Option<f64>,
        optional_nan: Option<f64>,
        optional_finite: Option<f64>,
        nested: Vec<Vec<f64>>,
        map: BTreeMap<String, Vec<f32>>,
        tuple: (f64, f32),
        newtype: Newtype,
        tuple_struct: Pair,
        variants: Vec<Variant>,
        #[serde(flatten)]
        flattened: BTreeMap<String, f64>,
    }

    /// 非有限の `f32` と、`Option<f64>` の `None` (null) と `Some(NaN)` ("NaN") を区別する。
    /// どの入れ子の形でも同じ扱いにする。
    #[test]
    fn non_finite_values_in_every_serde_shape_become_strings() {
        let shapes = Shapes {
            single: f32::NAN,
            optional_none: None,
            optional_nan: Some(f64::NAN),
            optional_finite: Some(2.5),
            nested: vec![vec![f64::INFINITY, 1.0], vec![f64::NEG_INFINITY]],
            map: BTreeMap::from([("k".to_string(), vec![f32::NEG_INFINITY, 0.5])]),
            tuple: (f64::NAN, f32::INFINITY),
            newtype: Newtype(f64::NEG_INFINITY),
            tuple_struct: Pair(f64::NAN, f32::NEG_INFINITY),
            variants: vec![
                Variant::Newtype(f64::NAN),
                Variant::Tuple(f64::INFINITY, f32::NAN),
                Variant::Struct {
                    value: f64::NEG_INFINITY,
                },
                Variant::Unit,
            ],
            flattened: BTreeMap::from([("flat".to_string(), f64::INFINITY)]),
        };
        let written: serde_json::Value = serde_json::from_slice(&to_vec(&shapes).unwrap()).unwrap();
        assert_eq!(
            written,
            serde_json::json!({
                "single": "NaN",
                "optional_none": null,
                "optional_nan": "NaN",
                "optional_finite": 2.5,
                "nested": [["Infinity", 1.0], ["-Infinity"]],
                "map": {"k": ["-Infinity", 0.5]},
                "tuple": ["NaN", "Infinity"],
                "newtype": "-Infinity",
                "tuple_struct": ["NaN", "-Infinity"],
                "variants": [
                    {"Newtype": "NaN"},
                    {"Tuple": ["Infinity", "NaN"]},
                    {"Struct": {"value": "-Infinity"}},
                    "Unit"
                ],
                "flat": "Infinity"
            })
        );
    }

    /// 有限の `f32` は serde_json の `f32` 用の書き方 (`0.1` を `0.10000000149011612` に
    /// しない) のまま変えない。
    #[test]
    fn finite_f32_values_are_written_exactly_as_before() {
        let values = [
            0.1_f32,
            -0.0,
            f32::MIN_POSITIVE,
            f32::MAX,
            f32::MIN,
            1.0e-45,
        ];
        assert_eq!(
            to_vec(&values).unwrap(),
            serde_json::to_vec(&values).unwrap()
        );
        assert_eq!(to_vec(&0.1_f32).unwrap(), b"0.1");
    }
}

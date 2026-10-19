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
}

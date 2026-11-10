//! What each dialect's words mean, and in which releases.
//!
//! Highlighting colors a word as a keyword, function or type only when the
//! connected engine knows it, and warns when a statement uses a function or
//! keyword the engine's release does not have yet (or no longer has).
//!
//! The lists are written as text: whitespace-separated words, each with
//! optional markers:
//!
//! - `word@15` / `word@8.0.14`: available since that release
//! - `word!8.0.11`: removed in that release
//! - `word*`: worth a warning even mid-statement when unavailable. Without
//!   it, a gated keyword only warns when it starts a statement, since an
//!   unavailable keyword mid-statement is more likely a column of that name.
//!
//! Versions are only given where the release is certain. A word without one
//! is assumed available in every release Quarry connects to.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::dialect::{Dialect, Flavor, Version};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WordClass {
    Keyword,
    Function,
    Type,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub class: WordClass,
    pub since: Option<Version>,
    pub until: Option<Version>,
    pub strict: bool,
}

/// Whether a word exists in a given release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Available,
    /// Added in this later release.
    Since(Version),
    /// Removed in this release, at or before the connected one.
    Removed(Version),
}

impl Entry {
    pub fn availability(&self, flavor: &Flavor) -> Availability {
        let Some(version) = flavor.version else {
            return Availability::Available;
        };
        match (self.since, self.until) {
            (Some(since), _) if version < since => Availability::Since(since),
            (_, Some(until)) if version >= until => Availability::Removed(until),
            _ => Availability::Available,
        }
    }
}

/// A dialect's words. Lookups are case-insensitive.
#[derive(Debug, Default)]
pub struct Vocabulary {
    words: HashMap<String, Vec<Entry>>,
}

impl Vocabulary {
    /// The vocabulary for `dialect`, built once.
    pub fn of(dialect: Dialect) -> &'static Vocabulary {
        static CACHE: [OnceLock<Vocabulary>; 6] = [const { OnceLock::new() }; 6];
        let ix = Dialect::ALL
            .iter()
            .position(|d| *d == dialect)
            .expect("every dialect is in ALL");
        CACHE[ix].get_or_init(|| Vocabulary::build(dialect))
    }

    /// The entry for `word` used as `class`, if the dialect has one.
    pub fn get(&self, word: &str, class: WordClass) -> Option<&Entry> {
        self.words
            .get(&word.to_ascii_lowercase())?
            .iter()
            .find(|e| e.class == class)
    }

    pub fn is(&self, word: &str, class: WordClass) -> bool {
        self.get(word, class).is_some()
    }

    /// All words of a class, for completion lists and tests.
    pub fn words(&self, class: WordClass) -> impl Iterator<Item = (&str, &Entry)> {
        self.words.iter().flat_map(move |(word, entries)| {
            entries
                .iter()
                .filter(move |e| e.class == class)
                .map(move |e| (word.as_str(), e))
        })
    }

    fn build(dialect: Dialect) -> Self {
        let mut vocabulary = Vocabulary::default();
        // Dialect lists first: their gates win over the shared lists.
        for (class, text) in lists(dialect) {
            vocabulary.add(class, text);
        }
        vocabulary.add(WordClass::Keyword, COMMON_KEYWORDS);
        vocabulary.add(WordClass::Function, COMMON_FUNCTIONS);
        vocabulary.add(WordClass::Type, COMMON_TYPES);
        vocabulary
    }

    fn add(&mut self, class: WordClass, text: &str) {
        for item in text.split_whitespace() {
            let (item, strict) = match item.strip_suffix('*') {
                Some(rest) => (rest, true),
                None => (item, false),
            };
            let (word, since, until) = if let Some((word, v)) = item.split_once('@') {
                (word, Some(parse_version(v)), None)
            } else if let Some((word, v)) = item.split_once('!') {
                (word, None, Some(parse_version(v)))
            } else {
                (item, None, None)
            };
            let entries = self.words.entry(word.to_ascii_lowercase()).or_default();
            if entries.iter().all(|e| e.class != class) {
                entries.push(Entry {
                    class,
                    since,
                    until,
                    strict,
                });
            }
        }
    }
}

fn parse_version(text: &str) -> Version {
    Version::parse(text).unwrap_or_else(|| panic!("bad version {text:?} in a vocabulary list"))
}

fn lists(dialect: Dialect) -> Vec<(WordClass, &'static str)> {
    use WordClass::*;
    match dialect {
        Dialect::Postgres => vec![
            (Keyword, POSTGRES_KEYWORDS),
            (Function, POSTGRES_FUNCTIONS),
            (Type, POSTGRES_TYPES),
        ],
        Dialect::DuckDb => vec![
            (Keyword, DUCKDB_KEYWORDS),
            (Function, DUCKDB_FUNCTIONS),
            (Type, DUCKDB_TYPES),
        ],
        Dialect::Sqlite => vec![
            (Keyword, SQLITE_KEYWORDS),
            (Function, SQLITE_FUNCTIONS),
            (Type, SQLITE_TYPES),
        ],
        Dialect::MySql => vec![
            (Keyword, MYSQL_KEYWORDS),
            (Keyword, MYSQL_FAMILY_KEYWORDS),
            (Function, MYSQL_FUNCTIONS),
            (Function, MYSQL_FAMILY_FUNCTIONS),
            (Type, MYSQL_TYPES),
            (Type, MYSQL_FAMILY_TYPES),
        ],
        Dialect::MariaDb => vec![
            (Keyword, MARIADB_KEYWORDS),
            (Keyword, MYSQL_FAMILY_KEYWORDS),
            (Function, MARIADB_FUNCTIONS),
            (Function, MYSQL_FAMILY_FUNCTIONS),
            (Type, MARIADB_TYPES),
            (Type, MYSQL_FAMILY_TYPES),
        ],
        Dialect::ClickHouse => vec![
            (Keyword, CLICKHOUSE_KEYWORDS),
            (Function, CLICKHOUSE_FUNCTIONS),
            (Type, CLICKHOUSE_TYPES),
        ],
    }
}

// ── Shared ──────────────────────────────────────────────────────────────────

const COMMON_KEYWORDS: &str = "
add all alter and any as asc between both by cascade case cast check collate
column commit constraint create cross current current_date current_time
current_timestamp default delete desc describe distinct drop else end escape
exists explain false fetch first following for foreign from full grant group
having if in index inner insert interval into is join key last leading left
like limit natural next no not null nulls offset on only or order outer over
partition preceding primary range recursive references rename replace
restrict revoke right rollback row rows savepoint schema select set show some
table temporary then to trailing transaction trigger true truncate unbounded
union unique update using values view when where window with without
";

const COMMON_FUNCTIONS: &str = "
abs avg cast ceil coalesce count floor greatest least length lower ltrim max min
nullif replace round rtrim substr substring sum trim upper
";

const COMMON_TYPES: &str = "
bigint boolean char character date decimal double float int integer numeric
real smallint text time timestamp varchar
";

// ── PostgreSQL ──────────────────────────────────────────────────────────────

const POSTGRES_KEYWORDS: &str = "
abort access action aggregate also always analyse analyze array asymmetric
at attach authorization begin breadth@14 cache call called checkpoint class
close cluster collation comment comments committed concurrently configuration
conflict connection constraints content continue conversion copy cost csv
cube cursor cycle data database deallocate declare deferrable deferred
definer delimiter delimiters depends depth@14 detach dictionary disable
discard do document domain each enable encoding encrypted enum event except
exclude excluding exclusive execute extension external family filter
finalize force forward freeze function functions generated global granted
grouping groups handler header hold identity ilike immediate immutable
implicit import include including increment inherit inherits initially
inline inout input insensitive instead intersect invoker isnull isolation
language large lateral leakproof level listen load local location lock
locked logged mapping match matched@15 materialized maxvalue merge@15
method minvalue mode move name names new nothing notify notnull nowait
object of off oids old operator option options ordinality others out
overlaps overriding owned owner parallel parser partial passing password
period@18 placing plans policy prepare prepared preserve prior privileges
procedural procedure procedures program publication quote read reassign
recheck ref referencing refresh reindex relative release repeatable replica
reset restart returning returns role rollup routine routines rule schemas
scroll search@14 security sequence sequences serializable server session
session_user setof sets share similar simple skip snapshot sql stable
standalone start statement statistics stdin stdout storage stored strict
strip subscription support symmetric sysid system system_user@16 tables
tablesample tablespace temp template ties@13 transform treat trusted type
types uescape uncommitted unencrypted unknown unlisten unlogged until vacuum
valid validate validator variadic verbose version virtual@18 volatile
whitespace within work wrapper write xmlattributes yes zone
";

const POSTGRES_FUNCTIONS: &str = "
acos age any_value@16 array_agg array_append array_cat array_dims
array_fill array_length array_lower array_ndims array_position
array_positions array_prepend array_remove array_replace array_reverse@18
array_sample@16 array_shuffle@16 array_sort@18 array_to_json
array_to_string array_upper ascii asin atan atan2 bit_and bit_count@14
bit_length bit_or bit_xor@14 bool_and bool_or btrim cardinality casefold@18
cbrt char_length character_length chr clock_timestamp col_description
concat concat_ws corr cos cosh covar_pop covar_samp cume_dist
current_database current_schema current_setting current_user currval
date_add@16 date_bin@14 date_part date_subtract@16 date_trunc decode
degrees dense_rank encode every exp extract factorial first_value
format format_type gcd@13 gen_random_uuid@13 generate_series
generate_subscripts has_table_privilege initcap isfinite json_agg
json_array@16 json_array_elements json_array_elements_text
json_array_length json_arrayagg@16 json_build_array json_build_object
json_each json_each_text json_exists@17 json_extract_path
json_extract_path_text json_object json_object_agg json_object_keys
json_objectagg@16 json_populate_record json_query@17 json_scalar@17
json_serialize@17 json_strip_nulls json_table@17 json_to_record
json_typeof json_value@17 jsonb_agg jsonb_array_elements
jsonb_array_elements_text jsonb_array_length jsonb_build_array
jsonb_build_object jsonb_each jsonb_each_text jsonb_extract_path
jsonb_extract_path_text jsonb_insert jsonb_object_agg jsonb_object_keys
jsonb_path_exists jsonb_path_match jsonb_path_query jsonb_path_query_array
jsonb_path_query_first jsonb_path_query_tz@13 jsonb_populate_record
jsonb_pretty jsonb_set jsonb_set_lax@13 jsonb_strip_nulls jsonb_to_record
jsonb_typeof justify_days justify_hours justify_interval lag last_value lcm@13
lead left ln localtime localtimestamp log log10 lpad make_date make_interval
make_time make_timestamp make_timestamptz md5 min_scale@13 mod mode
nextval normalize@13 now nth_value ntile obj_description octet_length
overlay percent_rank percentile_cont percentile_disc pg_backend_pid
pg_cancel_backend pg_column_size pg_current_xact_id@13 pg_database_size
pg_get_constraintdef pg_get_functiondef pg_get_indexdef pg_get_viewdef
pg_indexes_size pg_input_error_info@16 pg_input_is_valid@16
pg_relation_size pg_size_pretty pg_sleep pg_sleep_for pg_sleep_until
pg_table_size pg_terminate_backend pg_total_relation_size pg_typeof
phraseto_tsquery pi plainto_tsquery position power quote_ident
quote_literal quote_nullable radians random random_normal@16 range_agg@14
range_intersect_agg@14 rank regexp_count@15 regexp_instr@15 regexp_like@15
regexp_match regexp_matches regexp_replace regexp_split_to_array
regexp_split_to_table regexp_substr@15 repeat reverse right row_number
row_to_json rpad set_config setseed setval sha224 sha256 sha384 sha512
sign sin sinh split_part sqrt starts_with statement_timestamp stddev
stddev_pop stddev_samp string_agg string_to_array string_to_table@14 strpos
tan tanh timeofday to_char to_date to_hex to_json to_jsonb to_number
to_timestamp to_tsquery to_tsvector transaction_timestamp translate
trim_array@14 trim_scale@13 trunc ts_headline ts_rank txid_current unistr@13
unnest uuid_extract_timestamp@17 uuid_extract_version@17 uuidv4@18
uuidv7@18 var_pop var_samp variance version websearch_to_tsquery
width_bucket
";

const POSTGRES_TYPES: &str = "
bigserial bit bool box bpchar bytea cidr circle datemultirange@14 daterange
float4 float8 inet int2 int4 int4multirange@14 int4range int8
int8multirange@14 int8range json jsonb line lseg macaddr macaddr8 money name
nummultirange@14 numrange oid path pg_lsn pg_snapshot@13 point polygon
precision record regclass regproc regtype serial smallserial timestamptz
timetz tsmultirange@14 tsquery tsrange tstzmultirange@14 tstzrange tsvector
txid_snapshot uuid varbit varying void xml
";

// ── DuckDB ──────────────────────────────────────────────────────────────────

const DUCKDB_KEYWORDS: &str = "
anti array asof attach begin by_name call checkpoint columns copy detach
except exclude export filter force function glob ilike import install
intersect lambda lateral load macro map matched@1.4 materialized
merge@1.4 nothing pivot pivot_longer pivot_wider positional pragma qualify
replace returning sample secret semi similar struct summarize table
tablesample temp type unpivot use vacuum
";

const DUCKDB_FUNCTIONS: &str = "
age any_value approx_count_distinct approx_quantile arg_max arg_min
argmax argmin array_agg array_length bit_count bool_and bool_or concat
concat_ws contains corr count_if countif current_database current_schema
current_setting damerau_levenshtein date_diff date_part date_sub date_trunc
datediff dayname dense_rank duckdb_columns duckdb_databases
duckdb_extensions duckdb_functions duckdb_schemas duckdb_settings
duckdb_tables duckdb_views element_at ends_with entropy epoch epoch_ms exp
first first_value flatten format from_json gen_random_uuid generate_series
glob group_concat hamming hash histogram ifnull instr isinf isnan jaccard
jaro_winkler_similarity json_array json_extract json_extract_string
json_group_array json_group_object json_keys json_object json_structure
json_transform json_type json_valid kurtosis lag last last_day last_value
lcase lead left len levenshtein list list_aggregate list_concat
list_contains list_distinct list_extract list_filter list_has_all
list_has_any list_reduce list_reverse list_sort list_transform list_value
ln log log10 log2 lpad make_date make_timestamp map_extract
map_from_entries map_keys map_values max_by md5 mean median min_by mode
monthname now nth_value ntile parquet_metadata parquet_schema percent_rank
pi position pow power prefix printf product quantile quantile_cont
quantile_disc random range rank read_blob read_csv read_csv_auto read_json
read_json_auto read_ndjson read_parquet read_text regexp_extract
regexp_extract_all regexp_matches regexp_replace regexp_split_to_array
repeat reverse right row_number rpad setseed sha256 sign skewness split_part
sqrt starts_with stddev str_split strftime string_agg string_split strlen
strpos strptime struct_extract struct_insert struct_pack suffix time_bucket
to_json today try_cast typeof ucase unnest uuid var_pop version xor
yearweek
";

const DUCKDB_TYPES: &str = "
bit bitstring blob bool bytea datetime enum float4 float8 hugeint int1 int2
int4 int8 json list long map short string struct timestamp_ms timestamp_ns
timestamp_s timestamptz timetz tinyint ubigint uhugeint uinteger union
usmallint utinyint uuid
";

// ── SQLite ──────────────────────────────────────────────────────────────────

const SQLITE_KEYWORDS: &str = "
abort action after always@3.31 analyze attach autoincrement before begin
conflict database deferrable deferred detach do@3.24 each exclude@3.28
exclusive fail filter@3.30 generated@3.31 glob groups@3.28 ignore immediate
indexed initially instead isnull materialized@3.35 notnull nothing@3.24
others@3.28 plan pragma query raise regexp reindex release returning@3.35*
rowid stored@3.31 strict@3.37* temp ties@3.28 vacuum virtual
";

const SQLITE_FUNCTIONS: &str = "
acos@3.35 asin@3.35 atan@3.35 atan2@3.35 ceiling@3.35 changes char
concat@3.44 concat_ws@3.44 cos@3.35 cosh@3.35 cume_dist date datetime
degrees@3.35 dense_rank exp@3.35 first_value format@3.38 glob group_concat
hex if@3.48 ifnull iif@3.32 instr json json_array json_array_length
json_each json_error_position@3.42 json_extract json_group_array
json_group_object json_insert json_object json_patch json_pretty@3.46
json_quote json_remove json_replace json_set json_tree json_type json_valid
jsonb@3.45 jsonb_array@3.45 jsonb_extract@3.45 jsonb_group_array@3.45
jsonb_group_object@3.45 jsonb_insert@3.45 jsonb_object@3.45
jsonb_patch@3.45 jsonb_remove@3.45 jsonb_replace@3.45 jsonb_set@3.45
julianday lag last_insert_rowid last_value lead like likelihood likely ln@3.35
load_extension log@3.35 log10@3.35 log2@3.35 mod@3.35 nth_value ntile
octet_length@3.43 percent_rank pi@3.35 pow@3.35 power@3.35 printf quote
radians@3.35 random randomblob rank row_number sign@3.35 sin@3.35 sinh@3.35
soundex sqlite_compileoption_get sqlite_compileoption_used sqlite_offset
sqlite_source_id sqlite_version sqrt@3.35 strftime string_agg@3.44
substring@3.34 tan@3.35 tanh@3.35 time timediff@3.43 total total_changes
trunc@3.35 typeof unhex@3.41 unicode unixepoch@3.38 unlikely zeroblob
";

const SQLITE_TYPES: &str = "
any@3.37 blob clob datetime int2 int8 mediumint tinyint
";

// ── MySQL and MariaDB ───────────────────────────────────────────────────────

/// What MySQL 5.7+ and MariaDB 10.5+ share.
const MYSQL_FAMILY_KEYWORDS: &str = "
after algorithm analyze auto_increment before begin binary call change
charset checksum collation columns comment committed continue copy
cursor data databases day_hour day_minute day_second declare definer
delayed deterministic div do dumpfile duplicate each elseif enclosed engine
engines escaped event events every exit fields flush force function global
handler high_priority hour_minute hour_second ignore infile inplace
invoker isolation iterate kill leave level lines load local lock loop
low_priority minute_second modifies modify mode outfile partitions
procedure processlist quick read reads regexp repair repeat repeatable
require resignal return returns rlike schedule second_microsecond security
serializable session share signal sounds sql sql_big_result
sql_calc_found_rows sql_no_cache sql_small_result starting starts status
straight_join subpartition tables terminated uncommitted unlock unsigned
until use variables warnings while write xor year_month zerofill
";

const MYSQL_KEYWORDS: &str = "
except@8.0.31* instant@8.0.12 intersect@8.0.31* invisible lateral@8.0.14*
locked@8.0.1 member@8.0.17 nowait@8.0.1 skip@8.0.1 sql_cache!8.0.3 srid
visible
";

const MARIADB_KEYWORDS: &str = "
except intersect nextval period returning sequence system_time versioning
";

const MYSQL_FAMILY_FUNCTIONS: &str = "
acos adddate addtime aes_decrypt aes_encrypt asin atan atan2 avg benchmark
bin bit_and bit_count bit_length bit_or bit_xor char_length
character_length compress concat concat_ws connection_id conv convert
convert_tz cos cot crc32 curdate current_date current_time
current_timestamp current_user curtime database date date_add date_format
date_sub datediff day dayname dayofmonth dayofweek dayofyear degrees
elt exp export_set extract field find_in_set format found_rows
from_base64 from_days from_unixtime get_format group_concat hex hour if
ifnull inet6_aton inet6_ntoa inet_aton inet_ntoa insert instr is_ipv4
is_ipv6 isnull json_array json_array_append json_array_insert
json_contains json_contains_path json_depth json_extract json_insert
json_keys json_length json_merge_patch json_merge_preserve json_object
json_quote json_remove json_replace json_search json_set json_type
json_unquote json_valid last_day last_insert_id lcase left ln load_file
localtime localtimestamp locate log log10 log2 lpad make_set makedate
maketime md5 microsecond mid minute mod month monthname now oct
octet_length ord period_add period_diff pi position pow power quarter quote
radians rand regexp_instr regexp_replace regexp_substr
release_lock repeat reverse right row_count rpad sec_to_time second
session_user sha sha1 sha2 sign sin sleep soundex space sqrt st_astext
st_distance st_geomfromtext st_x st_y std stddev stddev_pop stddev_samp
str_to_date strcmp subdate substring_index subtime sysdate system_user tan
time time_format time_to_sec timediff timestamp timestampadd
timestampdiff to_base64 to_days to_seconds truncate ucase uncompress unhex
unix_timestamp user utc_date utc_time utc_timestamp uuid uuid_short
var_pop var_samp variance version week weekday weekofyear year yearweek
";

const MYSQL_FUNCTIONS: &str = "
any_value bin_to_uuid@8.0 cume_dist@8.0 current_role@8.0 decode!8.0.3
dense_rank@8.0 des_decrypt!8.0.3 des_encrypt!8.0.3 encode!8.0.3
encrypt!8.0.3 first_value@8.0 grouping@8.0.1 is_uuid@8.0 json_arrayagg
json_objectagg json_overlaps@8.0.17 json_pretty json_schema_valid@8.0.17
json_schema_validation_report@8.0.17 json_storage_free@8.0
json_storage_size json_table@8.0.4 json_value@8.0.21 lag@8.0
last_value@8.0 lead@8.0 nth_value@8.0 ntile@8.0 password!8.0.11
percent_rank@8.0 random_bytes rank@8.0 regexp_instr@8.0.4
regexp_like@8.0.4 regexp_replace@8.0.4 regexp_substr@8.0.4 row_number@8.0
st_srid statement_digest@8.0 string_to_vector@9.0 uuid_to_bin@8.0
vector_dim@9.0 vector_to_string@9.0
";

const MARIADB_FUNCTIONS: &str = "
add_months@10.6.1 cume_dist dense_rank first_value json_arrayagg
json_array_intersect@11.2 json_detailed json_equals@10.7 json_exists
json_key_value@11.2 json_normalize@10.7 json_object_filter_keys@11.2
json_object_to_array@11.2 json_objectagg json_overlaps@10.9 json_pretty
json_query json_schema_valid@11.1 json_table@10.6 json_value lag lastval
last_value lead median natural_sort_key@10.7 nextval nth_value ntile
percent_rank percentile_cont percentile_disc random_bytes@10.10 rank
row_number setval
sformat@10.7 sys_guid@10.6.1 to_char@10.6 uuid_v4@11.7 uuid_v7@11.7
vec_distance_cosine@11.7 vec_distance_euclidean@11.7 vec_fromtext@11.7
vec_totext@11.7
";

const MYSQL_FAMILY_TYPES: &str = "
binary bit blob bool dec enum fixed geometry geometrycollection json
linestring longblob longtext mediumblob mediumint mediumtext multilinestring
multipoint multipolygon point polygon serial set signed tinyblob tinyint
tinytext unsigned varbinary year datetime
";

const MYSQL_TYPES: &str = "
vector@9.0
";

const MARIADB_TYPES: &str = "
inet4@10.10 inet6 uuid@10.7 vector@11.7
";

// ── ClickHouse ──────────────────────────────────────────────────────────────

const CLICKHOUSE_KEYWORDS: &str = "
alias anti array asof async attach cluster codec cube database databases
deduplicate detach dictionary distributed engine ephemeral except exchange
final fill format freeze global ilike interpolate intersect kill layout
lifetime live materialized modify move mutation optimize paste populate
prewhere qualify@24.4* query refresh@23.12 sample semi settings source step
sync system tables temporary totals ttl unfreeze use watch
";

const CLICKHOUSE_FUNCTIONS: &str = "
any anyLast argMax argMin array arrayAll arrayAvg arrayConcat arrayCount
arrayDistinct arrayElement arrayEnumerate arrayExists arrayFilter
arrayFlatten arrayJoin arrayMap arrayMax arrayMin arrayReverseSort
arraySlice arraySort arrayStringConcat arraySum arrayUniq arrayZip
assumeNotNull avgIf bar base64Decode base64Encode bitmapBuild
bitmapCardinality cityHash64 concat countIf currentDatabase currentUser
dateAdd dateDiff dateSub date_trunc dictGet dictGetOrDefault dictHas empty
extract extractAll farmHash64 file first_value formatDateTime
formatReadableQuantity formatReadableSize formatReadableTimeDelta
fromUnixTimestamp64Milli generateRandom generateUUIDv4 generateUUIDv7@24.5
getSetting groupArray groupBitmap groupUniqArray has hasAll hasAny hex
hostName if ifNull ignore indexOf intDiv IPv4NumToString IPv4StringToNum
isNotNull isNull isValidJSON joinGet JSONExtract JSONExtractBool
JSONExtractFloat JSONExtractInt JSONExtractKeys JSONExtractKeysAndValues
JSONExtractRaw JSONExtractString JSONHas JSONLength JSONType lagInFrame
last_value leadInFrame leftPad lowerUTF8 map mapContains mapKeys mapValues
match materialize maxIf MD5 median merge minIf modulo multiIf murmurHash3_64
neighbor notEmpty now now64 nth_value nullIf numbers parseDateTimeBestEffort
parseDateTimeBestEffortOrNull position positionCaseInsensitive quantile
quantileExact quantiles quantileTDigest rand rand64 randCanonical range rank
remote replaceAll replaceOne replaceRegexpAll replaceRegexpOne retention
rightPad row_number runningDifference s3 sequenceMatch SHA256 sipHash64
sleep sleepEachRow splitByChar splitByString substringUTF8 sumIf sumMap
throwIf timeSlot toDate toDate32 toDateTime toDateTime64 toDayOfMonth
toDayOfWeek toDecimal32 toDecimal64 toFloat32 toFloat64 toHour toInt16
toInt32 toInt64 toInt8 toIPv4 toIPv6 toJSONString toMinute toMonday toMonth
topK toSecond toStartOfDay toStartOfHour toStartOfInterval toStartOfMinute
toStartOfMonth toStartOfQuarter toStartOfWeek toStartOfYear toString
toTypeName toUInt16 toUInt32 toUInt64 toUInt8 toUnixTimestamp
toUnixTimestamp64Milli toUUID toYear toYYYYMM toYYYYMMDD transform
trimBoth trimLeft trimRight tuple tupleElement uniq uniqCombined uniqExact
uniqHLL12 upperUTF8 uptime url version windowFunnel xxHash64 yesterday
zeros
";

const CLICKHOUSE_TYPES: &str = "
AggregateFunction Bool Date32 DateTime DateTime64 Decimal128 Decimal256
Decimal32 Decimal64 Dynamic@24.5 Enum Enum16 Enum8 FixedString Float32
Float64 Int128 Int16 Int256 Int32 Int64 Int8 IPv4 IPv6 JSON
LowCardinality Map MultiPolygon Nested Nothing Nullable Point Polygon Ring
SimpleAggregateFunction String Tuple UInt128 UInt16 UInt256 UInt32 UInt64
UInt8 UUID Variant@24.1 Array
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_dialect_builds() {
        for dialect in Dialect::ALL {
            let vocabulary = Vocabulary::of(dialect);
            assert!(vocabulary.is("select", WordClass::Keyword), "{dialect}");
            assert!(vocabulary.is("COUNT", WordClass::Function), "{dialect}");
            assert!(vocabulary.words(WordClass::Type).count() > 10, "{dialect}");
        }
    }

    #[test]
    fn gates_by_release() {
        let pg = Vocabulary::of(Dialect::Postgres);
        let regexp_count = pg.get("regexp_count", WordClass::Function).unwrap();
        let pg14 = Flavor::new(Dialect::Postgres, "14.12");
        let pg15 = Flavor::new(Dialect::Postgres, "15.0");
        assert_eq!(
            regexp_count.availability(&pg14),
            Availability::Since(Version::new(15, 0, 0))
        );
        assert_eq!(regexp_count.availability(&pg15), Availability::Available);
        assert_eq!(
            regexp_count.availability(&Flavor::unversioned(Dialect::Postgres)),
            Availability::Available
        );

        let mysql = Vocabulary::of(Dialect::MySql);
        let password = mysql.get("password", WordClass::Function).unwrap();
        assert_eq!(
            password.availability(&Flavor::new(Dialect::MySql, "5.7.44")),
            Availability::Available
        );
        assert_eq!(
            password.availability(&Flavor::new(Dialect::MySql, "8.0.39")),
            Availability::Removed(Version::new(8, 0, 11))
        );
    }

    #[test]
    fn dialect_entries_override_shared_ones() {
        // `returning` is gated in SQLite but plain in PostgreSQL.
        let sqlite = Vocabulary::of(Dialect::Sqlite);
        let returning = sqlite.get("returning", WordClass::Keyword).unwrap();
        assert_eq!(returning.since, Some(Version::new(3, 35, 0)));
        let mysql = Vocabulary::of(Dialect::MySql);
        assert!(!mysql.is("returning", WordClass::Keyword));
        assert!(Vocabulary::of(Dialect::MariaDb).is("returning", WordClass::Keyword));
    }

    #[test]
    fn a_word_can_be_a_keyword_and_a_function() {
        let mysql = Vocabulary::of(Dialect::MySql);
        assert!(mysql.is("left", WordClass::Keyword));
        assert!(mysql.is("left", WordClass::Function));
        assert!(mysql.is("year", WordClass::Type));
        assert!(mysql.is("year", WordClass::Function));
    }
}

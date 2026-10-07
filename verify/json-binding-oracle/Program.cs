// Measures Jellyfin's actual converters without changing server data.
// .NET 10 SDK; JellyfinDirectory is a Jellyfin 12.2 portable installation:
// dotnet run --project verify/json-binding-oracle -p:JellyfinDirectory=/path/to/jellyfin
// JSON-lines output is checked in at crates/ferrofin-api/tests/data/json-binding/.
using System.ComponentModel;
using System.Globalization;
using System.Text.Json;
using Jellyfin.Extensions.Json;

var types = new Dictionary<string, Type>
{
    ["i8"] = typeof(sbyte), ["i16"] = typeof(short), ["i32"] = typeof(int),
    ["i64"] = typeof(long), ["u8"] = typeof(byte), ["u16"] = typeof(ushort),
    ["u32"] = typeof(uint), ["u64"] = typeof(ulong),
    ["f32"] = typeof(float), ["f64"] = typeof(double),
    ["string"] = typeof(string), ["bool"] = typeof(bool),
    ["guid"] = typeof(Guid), ["guid?"] = typeof(Guid?),
    ["date"] = typeof(DateTime), ["date?"] = typeof(DateTime?),
    ["i32?"] = typeof(int?), ["bool?"] = typeof(bool?),
    ["enum"] = typeof(TestEnum), ["enum?"] = typeof(TestEnum?),
    ["default_enum"] = typeof(DefaultEnum), ["flags"] = typeof(FlagEnum)
};
string[] numbers = ["0", "-0", "1", "+1", "01", "1.0", "1.50", "1e2", "1E+02",
    " 1", "1 ", "1\t", ".5", "1.", "NaN", "nan", "Infinity", "-Infinity", "+Infinity",
    "inf", "1e400", "1e40", "1e-400", "-1", "127", "128", "255", "256",
    "32767", "32768", "65535", "65536", "2147483647", "2147483648", "4294967295",
    "4294967296", "9223372036854775807", "9223372036854775808", "18446744073709551615",
    "18446744073709551616", "-9223372036854775808", "-9223372036854775809"];
string[] strings = ["", " ", "true", "First", "first", " first ", "First,Second",
    "First, First", "1,4", "nope", "999", "2022-01-01", "2022-01-01T00:00:00Z",
    "2022-01-01T00:00:00", "2022-01-01T00:00:00+02:00", " 2022-01-01T00:00:00Z ",
    "2022-01-01t00:00:00z", "2022-01-01 00:00:00Z", "2022-01-01T00:00:00.1234567890123456Z",
    "2022-01-01T00:00:00.12345678901234567Z", "0000-01-01", "10000-01-01",
    "00000000000000000000000000000001", "00000000-0000-0000-0000-000000000001",
    "{00000000-0000-0000-0000-000000000001}", "(00000000-0000-0000-0000-000000000001)",
    "{0x00000000,0x0000,0x0000,{0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x01}}",
    "urn:uuid:00000000-0000-0000-0000-000000000001"];
var inputs = new[] { "null", "true", "false", "{}", "[]", "[1]", "{\"x\":1}" }
    .Concat(numbers.Where(n => { try { using var d = JsonDocument.Parse(n); return true; } catch { return false; } }))
    .Concat(numbers.Concat(strings).Select(s => JsonSerializer.Serialize(s)))
    .Distinct();
foreach (var (name, type) in types)
{
    foreach (var input in inputs)
    {
        var row = new Dictionary<string, object?> { ["type"] = name, ["input"] = input };
        try
        {
            var result = JsonSerializer.Deserialize(input, type, JsonDefaults.Options);
            row["accepted"] = true;
            row["value"] = result switch
            {
                DateTime date => date.ToString("O", CultureInfo.InvariantCulture),
                IFormattable value => value.ToString(null, CultureInfo.InvariantCulture),
                _ => result?.ToString()
            };
        }
        catch (Exception e)
        {
            row["accepted"] = false;
            row["error"] = e.GetType().Name;
        }
        Console.WriteLine(JsonSerializer.Serialize(row));
    }
}
enum TestEnum { First = 1, Second = 4 }
[DefaultValue(DefaultEnum.First)] enum DefaultEnum { First = 1, Second = 4 }
[Flags] enum FlagEnum { First = 1, Second = 4 }

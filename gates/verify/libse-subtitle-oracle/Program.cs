// Runs Jellyfin 12.2's subtitle conversion path over libse 4.0.12, the oracle for
// Ferrofin's subtitle writers. .NET 10 SDK, with the Jellyfin image's locale:
// LC_ALL=en_US.UTF-8 dotnet run --project gates/verify/libse-subtitle-oracle -- \
//     <input> <srt|ass|ssa|vtt> <out-dir> [startTicks endTicks preserve(0|1)] [--debug]
// writes one file per writer Jellyfin can reach from that source, named by format.
// The outputs are checked in under crates/ferrofin-mediaencoding/tests/data/libse/
// expected/<input>/<variant>/. --probe <file> dumps the .NET primitives the writers
// rely on (KnownColor values, TimeSpan rounding, ColorTranslator, decimal formatting).
//
// Parse = SubtitleEditParser.Parse (reflection over libse, extension-matched formats,
// the first with ErrorCount == 0 wins), FilterEvents = SubtitleEncoder.FilterEvents,
// write = SubtitleEncoder.TryGetWriter + ToText(subtitle, "untitled"), bytes =
// Encoding.UTF8.GetBytes (no BOM). JsonWriter is Jellyfin's, copied verbatim.
using System.Globalization;
using System.Text;
using System.Text.Json;
using Nikse.SubtitleEdit.Core.Common;
using Nikse.SubtitleEdit.Core.SubtitleFormats;

if (args[0] == "--probe")
{
    Probe.Write(args[1]);
    return 0;
}

var input = args[0];
var ext = args[1];
var outDir = args[2];
var rest = args.Skip(3).Where(a => a != "--debug").ToArray();
var debug = args.Contains("--debug");
long startTicks = rest.Length > 0 ? long.Parse(rest[0], CultureInfo.InvariantCulture) : 0;
long endTicks = rest.Length > 1 ? long.Parse(rest[1], CultureInfo.InvariantCulture) : 0;
bool preserve = rest.Length > 2 && rest[2] == "1";
Directory.CreateDirectory(outDir);

// SubtitleEditParser.GetSubtitleFormatTypes
var types = new Dictionary<string, List<Type>>(StringComparer.OrdinalIgnoreCase);
foreach (var type in typeof(SubtitleFormat).Assembly.GetTypes())
{
    if (!type.IsSubclassOf(typeof(SubtitleFormat)) || type.IsAbstract)
    {
        continue;
    }

    try
    {
        var e = ((SubtitleFormat)Activator.CreateInstance(type, true)!).Extension.TrimStart('.');
        if (!string.IsNullOrEmpty(e))
        {
            if (!types.TryGetValue(e, out var list))
            {
                types[e] = [type];
            }
            else
            {
                list.Add(type);
            }
        }
    }
    catch (Exception)
    {
    }
}

if (!types.TryGetValue(ext, out var candidates))
{
    Console.Error.WriteLine($"Unsupported file extension: {ext}");
    return 2;
}

// Jellyfin.Extensions StreamExtensions.ReadAllLines: StreamReader(UTF8).ReadLine().
var lines = new List<string>();
using (var reader = new StreamReader(File.OpenRead(input), Encoding.UTF8))
{
    string line;
    while ((line = reader.ReadLine()) is not null)
    {
        lines.Add(line);
    }
}

// SubtitleEditParser.Parse, which throws when nothing parses, then
// SubtitleEncoder.FilterEvents, which may leave nothing. Each writer gets its own
// parse, as each Jellyfin request does: some writers change the subtitle they are given.
Subtitle Load(StringBuilder log)
{
    var subtitle = new Subtitle();
    foreach (var t in candidates)
    {
        var format = (SubtitleFormat)Activator.CreateInstance(t, true)!;
        format.LoadSubtitle(subtitle, lines, ext);
        log?.AppendLine($"tried {format.Name}: errors {format.ErrorCount}, paragraphs {subtitle.Paragraphs.Count}");
        if (format.ErrorCount == 0)
        {
            break;
        }
    }

    return subtitle;
}

Subtitle Filter(Subtitle subtitle)
{
    subtitle.Paragraphs.RemoveAll(i => (i.StartTime.TimeSpan.Ticks - startTicks) < 0 && (i.EndTime.TimeSpan.Ticks - startTicks) < 0);
    if (endTicks > 0)
    {
        subtitle.Paragraphs.RemoveAll(i => i.StartTime.TimeSpan.Ticks > endTicks);
    }

    if (!preserve)
    {
        foreach (var p in subtitle.Paragraphs)
        {
            p.StartTime = new TimeCode(TimeSpan.FromTicks(Math.Max(0, p.StartTime.TimeSpan.Ticks - startTicks)));
            p.EndTime = new TimeCode(TimeSpan.FromTicks(Math.Max(0, p.EndTime.TimeSpan.Ticks - startTicks)));
        }
    }

    return subtitle;
}

var parseLog = new StringBuilder();
var loaded = Load(parseLog);
if (loaded.Paragraphs.Count == 0)
{
    Console.Error.WriteLine("Unsupported format: " + ext);
    return 3;
}

var parsed = Filter(loaded);
if (debug)
{
    var model = new
    {
        parsed.Header,
        parsed.Footer,
        Paragraphs = parsed.Paragraphs.Select(p => new
        {
            p.Number, p.Text, Start = p.StartTime.TotalMilliseconds, End = p.EndTime.TotalMilliseconds,
            p.Extra, p.IsComment, p.Actor, p.Region, p.MarginL, p.MarginR, p.MarginV, p.Effect, p.Layer,
            p.Language, p.Style,
        }),
    };
    File.WriteAllText(Path.Combine(outDir, "model.json"), JsonSerializer.Serialize(model, new JsonSerializerOptions { WriteIndented = true }));
    File.WriteAllText(Path.Combine(outDir, "parse.log"), parseLog.ToString());
}

// SubtitleEncoder.TryGetWriter. GetSubtitles serves a source verbatim when the request
// is its own format, or ASS for an SSA source, so those writers are never reached; an
// SRT source reaches the SubRip writer through the `subrip` alias, and a WebVTT source
// the WebVTT writer through the `webvtt` alias.
var writers = new (string Name, Func<SubtitleFormat> Make)[]
{
    ("ass", () => new AdvancedSubStationAlpha()),
    ("json", () => new JsonWriter()),
    ("srt", () => new SubRip()),
    ("ssa", () => new SubStationAlpha()),
    ("vtt", () => new WebVTT()),
    ("ttml", () => new TimedText10()),
};
foreach (var (name, make) in writers)
{
    var verbatim = (ext.Equals(name, StringComparison.OrdinalIgnoreCase) && name != "srt" && name != "vtt")
        || (ext.Equals("ssa", StringComparison.OrdinalIgnoreCase) && name == "ass");
    if (verbatim)
    {
        continue;
    }

    var text = make().ToText(Filter(Load(null)), "untitled");
    File.WriteAllBytes(Path.Combine(outDir, name), Encoding.UTF8.GetBytes(text));
}

return 0;

/// <summary>Jellyfin 12.2 MediaBrowser.MediaEncoding/Subtitles/JsonWriter.cs, verbatim apart from the namespace.</summary>
public class JsonWriter : SubtitleFormat
{
    public override string Extension => ".json";

    public override string Name => "JSON Jellyfin";

    public override string ToText(Subtitle subtitle, string title)
    {
        using var ms = new MemoryStream();
        using (var writer = new Utf8JsonWriter(ms))
        {
            var trackevents = subtitle.Paragraphs;
            writer.WriteStartObject();
            writer.WriteStartArray("TrackEvents");

            for (int i = 0; i < trackevents.Count; i++)
            {
                var current = trackevents[i];
                writer.WriteStartObject();

                writer.WriteString("Id", current.Number.ToString(CultureInfo.InvariantCulture));
                writer.WriteString("Text", current.Text);
                writer.WriteNumber("StartPositionTicks", current.StartTime.TimeSpan.Ticks);
                writer.WriteNumber("EndPositionTicks", current.EndTime.TimeSpan.Ticks);

                writer.WriteEndObject();
            }

            writer.WriteEndArray();
            writer.WriteEndObject();

            writer.Flush();
        }

        return Encoding.UTF8.GetString(ms.GetBuffer(), 0, (int)ms.Length);
    }

    public override void LoadSubtitle(Subtitle subtitle, List<string> lines, string fileName)
        => throw new NotImplementedException();
}

/// <summary>The .NET values the Rust ports of System.Drawing, TimeSpan and decimal copy.</summary>
public static class Probe
{
    public static void Write(string path)
    {
        using var probe = new StreamWriter(path);
        foreach (System.Drawing.KnownColor k in Enum.GetValues(typeof(System.Drawing.KnownColor)))
        {
            var c = System.Drawing.Color.FromKnownColor(k);
            probe.WriteLine($"known\t{k}\t{(uint)c.ToArgb():X8}\t{c.IsSystemColor}");
        }

        foreach (var ms in new[] { 0.00015, 0.00025, -0.00015, 1.23456789, 998.7655, 0.1, 1.0005, 123456.7899, -5000.0, -0.5, 359999999.0, 360000000.0, 86400000.0 })
        {
            var ts = TimeSpan.FromMilliseconds(ms);
            probe.WriteLine($"fromms\t{ms.ToString("R", CultureInfo.InvariantCulture)}\t{ts.Ticks}\t{ts.Days}\t{ts.Hours}\t{ts.Minutes}\t{ts.Seconds}\t{ts.Milliseconds}");
        }

        foreach (var ticks in new[] { 10005000L, 15L, 9987655L, 1L, 12345678L, 10000L, 5L })
        {
            var tc = new TimeCode(TimeSpan.FromTicks(ticks));
            probe.WriteLine($"fromticks\t{ticks}\t{tc.TotalMilliseconds.ToString("R", CultureInfo.InvariantCulture)}\t{tc.TimeSpan.Ticks}\t{tc.TotalSeconds.ToString("0.0##", CultureInfo.InvariantCulture)}");
        }

        foreach (var html in new[] { "#ff0000", "#f00", "#ff0000ff", "#1", "#", "#12345", "#1234567890", "red", "Red", "LightGrey", "#ABCDEF", "notacolor", "0x00ff00", "&h00ff00", "12,34,56", "255", "'red'", "transparent", "Control", "buttonface" })
        {
            try
            {
                var c = System.Drawing.ColorTranslator.FromHtml(html);
                probe.WriteLine($"fromhtml\t{html}\t{(uint)c.ToArgb():X8}\t{c.IsKnownColor}");
            }
            catch (Exception ex)
            {
                probe.WriteLine($"fromhtml\t{html}\tthrows {ex.GetType().Name}");
            }
        }

        foreach (var d in new[] { "1", "1.00", "0.5", "28", "28.75", "1.250", ".5", "5.", "00.10" })
        {
            var v = decimal.Parse(d, NumberStyles.AllowDecimalPoint, CultureInfo.InvariantCulture);
            probe.WriteLine($"decimal\t{d}\t{v.ToString(CultureInfo.InvariantCulture)}\t{v.ToString("0.#", CultureInfo.InvariantCulture)}\t{string.Format("{0:0.##}", v)}");
        }

        foreach (var s in new[] { " 12", "12 ", "+12", "-0", "1_0", "١٢", "0012", "2147483648", "", "12\0", "12 \0", "12\0 " })
        {
            probe.WriteLine($"isint\t[{s}]\t{int.TryParse(s, out _)}");
        }

        // decimal.TryParse(AllowDecimalPoint): scale is capped at 28 and the mantissa at 96 bits.
        foreach (var d in new[]
                 {
                     "0.00000000000000000000000000000000000001", "0.000000000000000000000000000045",
                     "0.000000000000000000000000000050", "0.000000000000000000000000000150",
                     "0.000000000000000000000000000250", "0.000000000000000000000000000051",
                     "1.2345678901234567890123456789", "9.9999999999999999999999999999",
                     "1.23456789012345678901234567895", "1.23456789012345678901234567885",
                     "79228162514264337593543950335", "79228162514264337593543950336",
                     "79228162514264337593543950335.4", "79228162514264337593543950335.5",
                     "7922816251426433759354395033.55", "123456789012345678901234567890.5",
                     "0000000000000000000000000000000000000001.5", "1.0000000000000000000000000000000000000000",
                 })
        {
            var ok = decimal.TryParse(d, NumberStyles.AllowDecimalPoint, CultureInfo.InvariantCulture, out var v);
            probe.WriteLine(ok
                ? $"decparse\t{d}\t{v.ToString(CultureInfo.InvariantCulture)}\t{v.ToString("0.#", CultureInfo.InvariantCulture)}"
                : $"decparse\t{d}\tfalse");
        }

        foreach (var c in "5٣²½Ⅷⅷé中ª\u0345a_")
        {
            probe.WriteLine($"charclass\t{c}\tU+{(int)c:X4}\t{char.IsDigit(c)}\t{char.IsLetter(c)}\t{char.IsLetterOrDigit(c)}");
        }

        // Convert.ToInt32(s, 16) (ColorTranslator's hex pieces) and int.TryParse(HexNumber)
        // (libse's alpha digits).
        foreach (var h in new[] { "ff", "+f", "-f", " f", "f ", "0x1f", "0X1F", "+0x1f", "0x", "x1", "ffffffff", "fffffffff", "", "+", "日0", "0000000041", "000000000ff", "ff\0", "ff\0\0", "ff\0 ", "00000000000" })
        {
            string convert;
            try
            {
                convert = Convert.ToInt32(h, 16).ToString(CultureInfo.InvariantCulture);
            }
            catch (Exception ex)
            {
                convert = "throws " + ex.GetType().Name;
            }

            var ok = int.TryParse(h, NumberStyles.HexNumber, null, out var hexValue);
            probe.WriteLine($"hex\t[{h}]\t{convert}\t{(ok ? hexValue.ToString(CultureInfo.InvariantCulture) : "false")}");
        }

        // float.TryParse(AllowDecimalPoint) also takes the invariant NaN/Infinity symbols.
        foreach (var f in new[] { "NaN", "nan", "Infinity", "infinity", "-Infinity", "+Infinity", "∞", "-∞", "1e5", " 1", "1.", ".", "-1", "30", " NaN", "NaN ", "\tNaN", "+NaN", "-NaN", " Infinity", "- Infinity", "+ NaN", "1\0", "NaN\0", "Infinity\0", " NaN\0" })
        {
            var ok = float.TryParse(f, NumberStyles.AllowDecimalPoint, CultureInfo.InvariantCulture, out var v);
            probe.WriteLine($"floatparse\t[{f}]\t{ok}\t{v.ToString(CultureInfo.InvariantCulture)}");
        }
    }
}

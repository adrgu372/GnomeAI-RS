using System.Text.Json;
namespace GnomeAI.Client;
public sealed record MessageContent(string Text,IReadOnlyList<string> Images)
{
    public static MessageContent Read(string raw) {
        if(!raw.TrimStart().StartsWith("[",StringComparison.Ordinal))return new(raw,[]);
        try {
            using var doc=JsonDocument.Parse(raw);
            if(doc.RootElement.ValueKind!=JsonValueKind.Array)return new(raw,[]);
            var text=new List<string>();var images=new List<string>();var recognized=false;
            foreach(var part in doc.RootElement.EnumerateArray()) {
                if(part.ValueKind!=JsonValueKind.Object || !part.TryGetProperty("type",out var type))continue;
                if(type.GetString()=="text" && part.TryGetProperty("text",out var value)){text.Add(value.GetString()??"");recognized=true;}
                if(type.GetString()=="image_url" && part.TryGetProperty("image_url",out var image) && image.ValueKind==JsonValueKind.Object && image.TryGetProperty("url",out var url)) {
                    recognized=true;var data=url.GetString()??"";
                    if(data.StartsWith("data:image/",StringComparison.Ordinal) && data.Length<=24*1024*1024 && images.Count<8)images.Add(data);
                    else text.Add("[Photo unavailable]");
                }
            }
            return recognized?new(string.Join("\n",text),images):new(raw,[]);
        }catch(Exception error) when(error is JsonException or InvalidOperationException){return new(raw,[]);}
    }
}

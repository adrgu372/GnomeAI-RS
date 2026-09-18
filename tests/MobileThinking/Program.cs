using GnomeAI.Android.UI;
var home=Path.Combine(Path.GetTempPath(),"gnomeai-thinking-"+Guid.NewGuid());
void Check(bool ok,string name){if(!ok)throw new Exception(name);Console.WriteLine("PASS: "+name);}
try {
var store=new MobileThinkingStore(home);var key=MobileThinkingStore.TurnKey(1,"answer");
await store.SaveAsync("phone/session",new(){{key,"reasoning șț"}});
Check((await new MobileThinkingStore(home).LoadAsync("phone/session"))[key]=="reasoning șț","reasoning survives reopening");
Check((await store.LoadAsync("other/session")).Count==0,"device/session isolation");
Check(key!=MobileThinkingStore.TurnKey(1,"edited answer") && key!=MobileThinkingStore.TurnKey(3,"answer"),"answer and turn identity");
await Task.WhenAll(Enumerable.Range(0,10).Select(i=>store.SaveAsync("phone/session",new(){{key,i.ToString()}})));
Check((await store.LoadAsync("phone/session")).ContainsKey(key) && Directory.GetFiles(home,"*.tmp",SearchOption.AllDirectories).Length==0,"concurrent saves leave complete JSON and no temporary files");
} finally {if(Directory.Exists(home))Directory.Delete(home,true);}

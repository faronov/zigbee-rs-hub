//! The hub's embedded web UI (`web/index.html`): a static page (HTML, CSS
//! and JS, no framework, no CDN) that talks only to the WebSocket JSON API
//! on `/api` (device-support 14), and the rarely used tabs it loads when
//! they are first opened (`web/ui/<name>.html`, [`MODULES`]). Served at `/`
//! and `/ui/<build>/<name>.js` by the firmware's esp_http_server and by
//! `hubctl serve`, pre-compressed.
#![no_std]

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod bundle;
#[cfg(test)]
mod minify;
#[cfg(test)]
mod variant;

/// The page, gzip-compressed at build time (`Content-Encoding: gzip`).
pub const INDEX_HTML_GZ: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/index.html.gz"));

/// Budget of the compressed page in every image variant (raised from
/// 32 KiB for the 8 MB image's OTA and reporting UI, from 34 KiB for the
/// device page's event entities and the catalog update before shards;
/// since then the rare tabs are [`MODULES`]); it counts in the firmware
/// image.
pub const PAGE_BUDGET: usize = 36 * 1024;

/// Budgets of the compressed modules (in any variant): Converters,
/// Catalog, the ZCL console, Updates, the migration wizard.
pub const MODULE_BUDGETS: [(&str, usize); 5] =
    [("conv", 3 * 1024), ("catalog", 4 * 1024), ("zcl", 4 * 1024), ("ota", 3 * 1024), ("mig", 4 * 1024)];

/// Budget of the page and all its modules, compressed, in every variant
/// but the developer edition: what the web UI takes of the firmware image
/// (each file compressed alone costs some 3 KiB more than one page; raised
/// from 44 KiB for the migration wizard's module).
pub const TOTAL_BUDGET: usize = 48 * 1024;

/// [`TOTAL_BUDGET`] of the developer edition ("devtools": feature `dev`,
/// the `tbr-hub-<ver>-8mb-devtools` and Dongle-M devtools images), with
/// room for its developer tools (raised from 64 KiB with [`TOTAL_BUDGET`]).
pub const DEV_TOTAL_BUDGET: usize = 68 * 1024;

/// Strong ETag of [`INDEX_HTML_GZ`], with its quotes.
pub const ETAG: &str = include_str!(concat!(env!("OUT_DIR"), "/etag"));

/// Response headers of the page (besides the status line).
pub const HEADERS: [(&str, &str); 4] = [
    ("Content-Type", "text/html; charset=utf-8"),
    ("Content-Encoding", "gzip"),
    ("Cache-Control", "no-cache"),
    ("ETag", ETAG),
];

/// Whether an `If-None-Match` header value matches the page (answer 304).
pub fn not_modified(if_none_match: &str) -> bool {
    matches(if_none_match, ETAG)
}

/// Whether an `If-None-Match` header value matches `etag` (quoted).
pub fn matches(if_none_match: &str, etag: &str) -> bool {
    if_none_match.trim() == "*" || if_none_match.split(',').any(|t| t.trim().trim_start_matches("W/") == etag)
}

/// Who may load a module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    /// Every page of this build (its tab is there).
    Always,
    /// Only while the hub's developer mode is on (`hub/developer/set`):
    /// the developer tools of the normal images (the developer edition
    /// has them [`Gate::Always`]).
    DeveloperMode,
}

/// A tab loaded on demand: a classic script for the page's scope, its
/// first line the build hash and the tab's markup.
#[derive(Debug)]
pub struct Module {
    /// The `data-mod` of its tab: `conv`, `catalog`, `zcl`, `ota`, `mig`
    /// (the migration wizard in Settings).
    pub name: &'static str,
    /// `/ui/<BUILD>/<name>.js`: of this build only, so a browser's cache
    /// never mixes builds; another build's path is not found (404) and the
    /// page reloads.
    pub path: &'static str,
    /// gzip-compressed (`Content-Encoding: gzip`).
    pub gz: &'static [u8],
    /// Strong ETag, with its quotes.
    pub etag: &'static str,
    pub gate: Gate,
}

impl Module {
    /// Whether to serve it now (else 403), with the hub's developer mode.
    pub fn served(&self, developer_mode: bool) -> bool {
        self.gate == Gate::Always || developer_mode
    }

    /// Its `Cache-Control`: kept for good (the path changes with every
    /// build); a gated one is asked again at every load (304 while the
    /// gate is open), so a browser's cache does not outlive the gate.
    pub fn cache_control(&self) -> &'static str {
        match self.gate {
            Gate::Always => "public, max-age=31536000, immutable",
            Gate::DeveloperMode => "no-cache",
        }
    }

    /// Response headers of a 200 (besides the status line).
    pub fn headers(&self) -> [(&'static str, &'static str); 4] {
        [
            ("Content-Type", "text/javascript; charset=utf-8"),
            ("Content-Encoding", "gzip"),
            ("Cache-Control", self.cache_control()),
            ("ETag", self.etag),
        ]
    }
}

include!(concat!(env!("OUT_DIR"), "/ui.rs"));

/// The module at a request path, of this build only.
pub fn module(path: &str) -> Option<&'static Module> {
    MODULES.iter().find(|m| m.path == path)
}

#[cfg(test)]
mod tests {
    use super::bundle::{self, Bundle};
    use super::variant::{Features, select};
    use super::*;
    use std::format;
    use std::string::{String, ToString};
    use std::vec::Vec;

    fn src(name: &str) -> String {
        match name {
            "index.html" => include_str!("../web/index.html"),
            "ui/conv.html" => include_str!("../web/ui/conv.html"),
            "ui/catalog.html" => include_str!("../web/ui/catalog.html"),
            "ui/zcl.html" => include_str!("../web/ui/zcl.html"),
            "ui/ota.html" => include_str!("../web/ui/ota.html"),
            "ui/mig.html" => include_str!("../web/ui/mig.html"),
            _ => panic!("no web/{name}"),
        }
        .to_string()
    }

    fn build(f: Features) -> Bundle {
        bundle::bundle(f, &src).unwrap_or_else(|e| panic!("{f:?}: {e}"))
    }

    /// The page and the modules of a variant as selected, not minified
    /// (for what a variant contains).
    fn text(f: Features) -> String {
        let mut t = select(&src("index.html"), &f.sections()).unwrap();
        for (name, _) in bundle::modules(f) {
            t.push_str(&select(&src(&format!("ui/{name}.html")), &f.sections()).unwrap());
        }
        t
    }

    const BUILT: Features = Features {
        advanced: cfg!(feature = "advanced"),
        ota: cfg!(feature = "ota"),
        ota_store: cfg!(feature = "ota-store"),
        gateway: cfg!(feature = "gateway"),
        dev: cfg!(feature = "dev"),
    };

    const PLAIN: Features = Features { advanced: false, ota: false, ota_store: false, gateway: false, dev: false };

    fn gunzip(gz: &[u8]) -> String {
        assert_eq!(&gz[..4], &[0x1f, 0x8b, 8, 0], "gzip, deflate, no optional fields");
        let body = &gz[10..gz.len() - 8];
        let data = miniz_oxide::inflate::decompress_to_vec(body).expect("deflate stream");
        let isize = u32::from_le_bytes(gz[gz.len() - 4..].try_into().unwrap());
        assert_eq!(isize as usize, data.len());
        String::from_utf8(data).unwrap()
    }

    /// The page's script.
    fn script(html: &str) -> String {
        let a = html.find("<script>").unwrap() + "<script>".len();
        html[a..a + html[a..].find("</script>").unwrap()].to_string()
    }

    /// Runs `js` in Node (`stdin` its input); true when Node is not installed.
    fn node(js: &str, stdin: &str) -> bool {
        use std::io::Write;
        let Ok(mut node) = std::process::Command::new("node")
            .args(["-e", js])
            .stdin(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        else {
            return true;
        };
        node.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
        let out = node.wait_with_output().unwrap();
        let _ = std::io::stderr().write_all(&out.stderr);
        out.status.success()
    }

    #[test]
    fn embedded_page_and_modules_are_the_build() {
        let b = build(BUILT);
        assert_eq!(gunzip(INDEX_HTML_GZ), b.page);
        assert_eq!(BUILD, b.build);
        assert_eq!(MODULES.len(), b.modules.len());
        for (m, want) in MODULES.iter().zip(&b.modules) {
            assert_eq!((m.name, gunzip(m.gz)), (want.name, want.js.clone()));
            assert_eq!(m.path, format!("/ui/{BUILD}/{}.js", m.name));
            assert_eq!(m.gate, if want.developer { Gate::DeveloperMode } else { Gate::Always });
            assert_eq!(m.etag, bundle::etag(m.gz));
        }
    }

    /// The page (the core) within [`PAGE_BUDGET`], each module within its
    /// budget and all of them within the total budget, in every variant
    /// (compressed as `build.rs` does); `--nocapture` prints the sizes.
    #[test]
    fn every_variant_fits_its_budgets() {
        std::eprintln!("variant: page + modules = total (gzip, B)");
        for f in Features::all() {
            let b = build(f);
            let page = bundle::gzip(b.page.as_bytes()).len();
            let mut total = page;
            let mut row = format!("page {page}");
            for m in &b.modules {
                let gz = bundle::gzip(m.js.as_bytes()).len();
                let budget = MODULE_BUDGETS.iter().find(|(n, _)| *n == m.name).expect("a module budget").1;
                assert!(gz <= budget, "{f:?}: module {} {gz} B compressed, budget {budget}", m.name);
                total += gz;
                row.push_str(&format!(" + {} {gz}", m.name));
            }
            assert!(page <= PAGE_BUDGET, "{f:?}: page {page} B compressed");
            let limit = if f.dev { DEV_TOTAL_BUDGET } else { TOTAL_BUDGET };
            assert!(total <= limit, "{f:?}: {total} B compressed in all, budget {limit}");
            std::eprintln!("{f:?}: {row} = {total}");
            if f == BUILT {
                assert_eq!(page, INDEX_HTML_GZ.len());
            }
        }
    }

    /// Every build has its own hash: the page's `UIB`, the path and the
    /// first line of every module (which the page checks before running
    /// it). A change to one module changes them all, so a browser's cache
    /// never mixes a firmware update's modules with older ones.
    #[test]
    fn modules_carry_the_build_hash() {
        let mut seen = std::collections::BTreeSet::new();
        for f in Features::all() {
            let b = build(f);
            assert!(seen.insert(b.build.clone()), "{f:?}: the same build hash as another variant");
            assert_eq!(b.build.len(), 16);
            assert_eq!(b.page.matches(&format!("UIB=\"{}\"", b.build)).count(), 1);
            assert!(!b.page.contains(bundle::BUILD_PLACEHOLDER));
            for m in &b.modules {
                assert!(m.js.starts_with(&bundle::header(&b.build, m.name)), "{}", m.name);
                assert!(b.page.contains(&format!("data-mod=\"{}\"", m.name)), "{f:?}: no tab for {}", m.name);
            }
        }
        let changed = |name: &str| {
            let alt = |n: &str| if n == name { src(n).replacen("<script>\n", "<script>\nlet zzChanged=1;\n", 1) } else { src(n) };
            bundle::bundle(BUILT, &alt).unwrap()
        };
        let b = build(BUILT);
        for name in ["index.html", "ui/conv.html", "ui/catalog.html"] {
            let c = changed(name);
            assert_ne!(c.build, b.build, "{name}");
            assert!(c.modules.iter().all(|m| !m.js.starts_with(&bundle::header(&b.build, m.name))));
        }
        for m in MODULES {
            assert!(module(m.path).is_some_and(|x| x.name == m.name));
            assert!(module(&m.path.replace(BUILD, "0000000000000000")).is_none(), "another build's module");
            assert!(m.etag.starts_with('"') && m.etag.ends_with('"') && m.etag.len() == 18 && matches(m.etag, m.etag));
        }
        assert!(module("/ui/x.js").is_none() && module("/").is_none());
    }

    /// The ZCL console: a module of the advanced images behind the hub's
    /// developer mode (the page shows its tab when hub/info says so and has
    /// the switch), always in the developer edition (no switch); none in
    /// the basic images.
    #[test]
    fn console_module_follows_developer_mode() {
        for f in Features::all() {
            let b = build(f);
            let zcl = b.modules.iter().find(|m| m.name == "zcl");
            let page = &b.page;
            match (f.advanced, f.dev) {
                (false, _) => assert!(zcl.is_none() && !page.contains("data-tab=\"zcl\"")),
                (true, false) => {
                    assert!(zcl.unwrap().developer);
                    assert!(page.contains("data-tab=\"zcl\" data-title=\"ZCL\" data-need=\"developer_mode\" data-mod=\"zcl\""));
                    assert!(page.contains("\"hub/developer/set\"") && page.contains("id=\"dvm\""));
                    assert!(!page.contains("Developer edition"));
                }
                (true, true) => {
                    assert!(!zcl.unwrap().developer);
                    assert!(page.contains("data-tab=\"zcl\" data-title=\"ZCL\" data-mod=\"zcl\""));
                    assert!(!page.contains("data-need=\"developer_mode\"") && !page.contains("id=\"dvm\""));
                    assert!(page.contains("Developer edition (devtools)"));
                }
            }
            assert!(b.modules.iter().filter(|m| m.name != "zcl").all(|m| !m.developer));
        }
        let m = |gate| Module { name: "x", path: "/ui/x/x.js", gz: &[], etag: "\"\"", gate };
        assert!(m(Gate::Always).served(false) && m(Gate::Always).served(true));
        assert!(!m(Gate::DeveloperMode).served(false) && m(Gate::DeveloperMode).served(true));
        // A gated module is not kept by the browser past the gate.
        assert!(m(Gate::Always).cache_control().contains("immutable"));
        assert_eq!(m(Gate::DeveloperMode).cache_control(), "no-cache");
    }

    /// The page's and the modules' scripts, minified and not, parse (when
    /// Node is installed).
    #[test]
    fn scripts_parse() {
        let mut scripts = Vec::new();
        for f in Features::all() {
            let b = build(f);
            scripts.push(script(&b.page));
            scripts.push(script(&select(&src("index.html"), &f.sections()).unwrap()));
            for m in b.modules {
                scripts.push(script(&select(&src(&format!("ui/{}.html", m.name)), &f.sections()).unwrap()));
                scripts.push(m.js);
            }
        }
        // One Node for all, the scripts separated by NUL.
        let check = "require('fs').readFileSync(0,'utf8').split('\\0').forEach((s,i)=>{try{new Function(s)}catch(e){throw Error('script '+i+': '+e.message)}})";
        assert!(scripts.iter().all(|s| !s.contains('\0')));
        assert!(node(check, &scripts.join("\0")), "a script does not parse");
    }

    /// The page and its modules share one scope: run in Node against a
    /// stand-in DOM, every variant's page and then its modules (in both
    /// orders) declare nothing twice, run their first statement (`uiMod` of
    /// the page's build) and define what the page calls into (when Node is
    /// installed).
    #[test]
    fn modules_join_the_page_scope() {
        let harness = r#"
const vm=require("vm"),S=JSON.parse(require("fs").readFileSync(0,"utf8"));
const any=()=>new Proxy(function(){},{get:(t,k)=>k===Symbol.toPrimitive?()=>"":k=="then"?undefined:any(),set:()=>true,apply:()=>any(),construct:()=>any()});
for(const order of [S.modules,S.modules.slice().reverse()]){
 const g={console,setTimeout:()=>0,clearTimeout(){},setInterval:()=>0,document:any(),navigator:{},localStorage:{getItem:()=>null,setItem(){},removeItem(){}},sessionStorage:{},
  location:{hash:"",protocol:"http:",host:"hub",pathname:"/",reload(){throw Error("reload")}},history:{replaceState(){}},addEventListener(){},WebSocket:function(){return any()},Option:function(){},
  fetch:()=>new Promise(()=>{}),Blob:function(){},URL:{createObjectURL:()=>""},btoa:s=>s,atob:s=>s,onbeforeunload:null,confirm:()=>false,prompt:()=>null};
 vm.createContext(g);vm.runInContext(S.page,g,{filename:"index.html"});
 for(const[n,js]of order)vm.runInContext(js,g,{filename:n+".js"});
 for(const n of S.need)if(vm.runInContext("typeof "+n,g)!="function")throw Error(S.variant+": "+n+" is not a function after the modules");
}"#;
        let json = |s: &str| {
            let mut o = String::from("\"");
            for c in s.chars() {
                match c {
                    '"' => o.push_str("\\\""),
                    '\\' => o.push_str("\\\\"),
                    c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
                    c => o.push(c),
                }
            }
            o.push('"');
            o
        };
        for f in Features::all() {
            let b = build(f);
            let mut need = std::vec!["ctEv", "ctFind", "ctSerial"];
            for m in &b.modules {
                need.push(match m.name {
                    "conv" => "onTab.conv",
                    "catalog" => "onTab.catalog",
                    "zcl" => "zFrame",
                    "mig" => "mgRead",
                    _ => "otEv",
                });
            }
            if f.advanced || f.dev {
                need.push("onTab.zcl");
            }
            if f.ota || f.ota_store {
                need.extend(["onTab.ota", "otRe", "otNeed"]);
            }
            let modules: Vec<String> = b.modules.iter().map(|m| format!("[{},{}]", json(m.name), json(&m.js))).collect();
            let need: Vec<String> = need.iter().map(|n| json(n)).collect();
            let input = format!(
                "{{\"variant\":{},\"page\":{},\"modules\":[{}],\"need\":[{}]}}",
                json(&format!("{f:?}")),
                json(&script(&b.page)),
                modules.join(","),
                need.join(",")
            );
            assert!(node(harness, &input), "{f:?}: page and modules do not share their scope");
        }
    }

    /// The reporting panel's change conversion, raw ⇄ entity units (the
    /// record's `scale`), run in Node when it is installed.
    #[test]
    fn reporting_change_converts_between_raw_and_entity_units() {
        let src = include_str!("../web/index.html");
        let helpers = src
            .lines()
            .find(|l| l.starts_with("const chU="))
            .expect("chU/chR line");
        let checks = r#"
const eq=(a,b)=>{if(!Object.is(a,b))throw new Error(a+" != "+b)};
// raw → entity: divisor, multiplier, fractional factors, float noise cleaned
eq(chU(50,0.01),0.5);eq(chU(100,0.01),1);eq(chU(3,0.1),0.3);eq(chU(2,0.5),1);
eq(chU(7,2.5),17.5);eq(chU(3,10),30);eq(chU(1,0.125),0.125);eq(chU(4294967295,1),4294967295);
// entity → raw, integer types: rounded to a whole raw unit
eq(chR("0.5",0.01,false),50);eq(chR("0,5",0.01,false),50);eq(chR("0.333",0.01,false),33);
eq(chR("0.337",0.01,false),34);eq(chR("0.004",0.01,false),0);eq(chR("1",0.5,false),2);
eq(chR("0.7",0.5,false),1);eq(chR("25",10,false),3);eq(chR("24",10,false),2);eq(chR("2",2.5,false),1);
// float types keep the fraction (to 9 decimals)
eq(chR("0.05",0.1,true),0.5);eq(chR("0.3",0.1,true),3);eq(chR("1",3,true),0.333333333);
// round trip through the display
for(const k of [0.01,0.1,0.5,2.5,10])for(const c of [0,1,7,50,999])eq(chR(String(chU(c,k)),k,false),c);
// not a number: NaN (the page then sends the text, which the hub refuses)
eq(Number.isNaN(chR("x",1,false)),true);
"#;
        assert!(node(&format!("{helpers}\n{checks}"), ""), "conversion checks failed");
    }

    /// The time zone picker's IANA → POSIX conversion (`tzPosix`) with this
    /// Node's Intl data (when Node is installed): rules the hub's parser
    /// takes (`clock.rs`), checked hour by hour against Intl, and today's
    /// offset, fixed, for changes on no yearly rule.
    #[test]
    fn time_zones_become_posix_rules() {
        let src = include_str!("../web/index.html");
        let (a, b) = (src.find("const tzF=").unwrap(), src.find("// The zone picked here").unwrap());
        let checks = r#"
const eq=(a,b)=>{a=JSON.stringify(a);b=JSON.stringify(b);if(a!==b)throw new Error(a+" != "+b)};
const t=z=>tzPosix(z,2026);
// Intl's short names where they are letters, else <+hh[mm]>; times other than 02:00 written out.
eq(t("Europe/Tallinn"),{tz:"EET-2EEST,M3.5.0/3,M10.5.0/4"});
eq(t("Europe/Kyiv"),{tz:"EET-2EEST,M3.5.0/3,M10.5.0/4"});
eq(t("Europe/London"),{tz:"GMT0BST,M3.5.0/1,M10.5.0"});
eq(t("America/New_York"),{tz:"EST5EDT,M3.2.0,M11.1.0"});
// A 30 min shift; offsets and times with minutes.
eq(t("Australia/Lord_Howe"),{tz:"<+1030>-10:30<+11>-11,M10.1.0,M4.1.0"});
eq(t("Pacific/Chatham"),{tz:"<+1245>-12:45<+1345>,M9.5.0/2:45,M4.1.0/3:45"});
// Changes a day off the rule's weekday: the Friday before the last Sunday
// (not the last Friday, which differs in 2028), midnight after the first Saturday.
eq(t("Asia/Jerusalem"),{tz:"<+02>-2<+03>,M3.4.4/26,M10.5.0"});
eq(t("America/Santiago"),{tz:"<-04>4<-03>,M9.1.6/24,M4.1.6/24"});
// No daylight saving time.
eq(t("America/Sao_Paulo"),{tz:"<-03>3",fixed:false});
eq(t("Asia/Kolkata"),{tz:"<+0530>-5:30",fixed:false});
eq(t("UTC"),{tz:"UTC0",fixed:false});
// 2026 in Morocco: Ramadan changes (and with tzdata 2026c a move to
// permanent +00 in September) follow no yearly rule: today's offset,
// fixed; its value and name depend on this Node's data (<+01>-1, WET0).
const c=t("Africa/Casablanca");eq(c.fixed,true);if(!/^(<[+-]\d\d>|[A-Z]{3,})-?\d$/i.test(c.tz))throw new Error(c.tz);
"#;
        use std::io::Write;
        let Ok(node) = std::process::Command::new("node")
            .args(["-e", &std::format!("{}\n{checks}", &src[a..b])])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
        else {
            return;
        };
        let out = node.wait_with_output().unwrap();
        let _ = std::io::stderr().write_all(&out.stderr);
        assert!(out.status.success(), "time zone conversion checks failed");
    }

    /// The migration wizard's Zigbee2MQTT reader (when Node is installed):
    /// `database.db` lines (a corrupt one, the coordinator, a group),
    /// friendly names of `configuration.yaml` and of a `devices.yaml`, the
    /// backup, into the import document the hub takes.
    #[test]
    fn migration_reads_zigbee2mqtt_files() {
        let src = include_str!("../web/ui/mig.html");
        let js = &src[src.find("<script>").unwrap() + 8..src.find("</script>").unwrap()];
        let checks = r#"
const eq=(a,b)=>{a=JSON.stringify(a);b=JSON.stringify(b);if(a!==b)throw new Error(a+" != "+b)};
const file=(name,t)=>({name,text:async()=>t});
const db=[
 {id:1,type:"Coordinator",ieeeAddr:"0x00124b00c0ffee01",nwkAddr:0,endpoints:{}},
 {id:2,type:"Router",ieeeAddr:"0x0017880100F1E2D3",nwkAddr:31248,manufId:4107,manufName:"Signify",powerSource:"Mains (single phase)",modelId:"LCT015",
  endpoints:{"11":{profId:260,epId:11,devId:269,inClusterList:[0,6,768],outClusterList:[25],clusters:{lightingColorCtrl:{attributes:{colorCapabilities:31,currentX:9}}}},
             "2":{profId:260,epId:2,devId:1,inClusterList:[],outClusterList:[]}},swBuildId:"1.50",interviewCompleted:true,interviewState:"SUCCESSFUL"},
 {id:3,type:"EndDevice",ieeeAddr:"0x00158d0003c4d5e6",nwkAddr:1202,powerSource:"Battery",endpoints:{},interviewCompleted:false},
 {id:5,type:"Router",ieeeAddr:"0x00124b00aabbcc01",nwkAddr:4369,endpoints:{"1":{profId:260,epId:1,devId:0,inClusterList:[0,6],outClusterList:[]}},interviewState:"FAILED"},
 {id:6,type:"Router",ieeeAddr:"0x00124b00aabbcc02",nwkAddr:4370,endpoints:{}},
 {id:4,type:"Group",groupID:4097,members:[{deviceIeeeAddr:"0x0017880100f1e2d3",endpointID:11}]}
].map(e=>JSON.stringify(e)).join("\n")+"\nnot json\n";
const conf="homeassistant: true\ndevices:\n  '0x0017880100f1e2d3':\n    friendly_name: Hall light\n    retain: false\ngroups:\n  '4097':\n    friendly_name: Hall\nmqtt:\n  server: mqtt://x\n";
const devs="'0x00158d0003c4d5e6':\n  friendly_name: 'Kitchen/temperature'\n";
const ocb={metadata:{format:"zigpy/open-coordinator-backup",version:1,source:"zigbee-herdsman@3.2.0"},extended_pan_id:"a1b2c3d4e5f60718",devices:[]};
(async()=>{
 const d=await mgRead([file("database.db",db),file("configuration.yaml",conf),file("devices.yaml",devs),file("coordinator_backup.json",JSON.stringify(ocb))]);
 eq([d.format,d.version,d.source.kind,d.network.extended_pan_id],["zigbee-rs-hub/import",1,"z2m","a1b2c3d4e5f60718"]);
 eq(d.devices.length,4);
 // The source's interview state: herdsman ≥ 3's interviewState, older interviewCompleted, else unsaid.
 eq(d.devices.map(x=>x.interviewed),[true,false,false,void 0]);
 const l=d.devices[0];
 eq([l.ieee,l.nwk,l.name,l.type,l.power_source,l.sw_build_id],["0017880100f1e2d3",31248,"Hall light","router",1,"1.50"]);
 eq(l.endpoints.map(e=>e.id),[2,11]);
 eq(l.endpoints[1],{id:11,profile:260,device_id:269,in:[0,6,768],out:[25]});
 eq(l.attributes,[{ep:11,cluster:768,attr:16394,value:31}]);
 eq([d.devices[1].name,d.devices[1].type,d.devices[1].power_source],["Kitchen/temperature","end_device",3]);
 eq(d.groups,[{id:4097,name:"Hall",members:[{ieee:"0017880100f1e2d3",ep:11}]}]);
 // An import document passes through.
 eq((await mgRead([file("doc.json",JSON.stringify({format:"zigbee-rs-hub/import",version:1,devices:[]}))])).devices,[]);
})().catch(e=>{console.error(e);process.exit(1)});
"#;
        use std::io::Write;
        let Ok(node) = std::process::Command::new("node")
            .args(["-e", &std::format!("let toast=()=>{{}},onTab={{}},$=()=>({{}});\n{js}\n{checks}")])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
        else {
            return;
        };
        let out = node.wait_with_output().unwrap();
        let _ = std::io::stderr().write_all(&out.stderr);
        assert!(out.status.success(), "migration reader checks failed");
    }

    /// The Hub card with a `tz_name` this Intl does not know (the hub took
    /// it from newer tzdata): the hub facts still load, the hub's own time
    /// is shown, and Save keeps the name (the page's time code runs in Node
    /// with stubs of the DOM and the API, when Node is installed).
    #[test]
    fn unknown_time_zone_names_are_kept() {
        let src = include_str!("../web/index.html");
        let a = src.find("const tzF=").unwrap();
        let b = a + src[a..].find("$(\"#tzb\").onclick=").unwrap();
        let b = b + src[b..].find('\n').unwrap();
        let mock = r##"
const eq=(a,b)=>{a=JSON.stringify(a);b=JSON.stringify(b);if(a!==b)throw new Error(a+" != "+b)};
const E={},$=s=>E[s]||(E[s]={value:"",textContent:"",classList:{add(){}},append(){}}),document={activeElement:null};
globalThis.Option=function(v){this.value=v};
let sys=0,sent,toasts=[];const loadSys=()=>sys++,toast=m=>toasts.push(m),busy=(_,p)=>p;
const HUB={tz:"EET-2EEST,M3.5.0/3,M10.5.0/4",local:"2026-11-25 10:00:00",time_synced:true,servers:["pool.ntp.org"]};
let rec;const call=async()=>rec,act=async(t,p)=>{sent=p;return rec};
"##;
        let checks = r##"
(async()=>{
 for(const [name,shown] of [["Mars/Olympus_Mons","Hub time 2026-11-25 10:00:00"],["Europe/Tallinn","Hub time with Europe/Tallinn: 20"]]){
  rec=Object.assign({tz_name:name},HUB);sys=0;toasts=[];
  loadTime();await new Promise(r=>setTimeout(r));
  eq([sys,$("#tzn").value,$("#tzm").textContent.startsWith(shown)],[1,name,true]);
  await $("#tzb").onclick({target:{}});
  eq([sent.tz,sent.tz_name,toasts,sys,$("#tzm").textContent.startsWith(shown)],[HUB.tz,name,["Time zone saved"],2,true]);
 }
})().catch(e=>{console.error(e);process.exit(1)});
"##;
        let js = std::format!("{mock}\n{}\n{checks}", &src[a..b]);
        use std::io::Write;
        let Ok(node) = std::process::Command::new("node")
            .args(["-e", &js])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
        else {
            return;
        };
        let out = node.wait_with_output().unwrap();
        let _ = std::io::stderr().write_all(&out.stderr);
        assert!(out.status.success(), "unknown time zone name checks failed");
    }

    /// Shard fetches against an older installed manifest (the mirror moved
    /// on): the page reads the signed manifest's version, installs a newer
    /// one first or after a refused shard and retries once; an older or the
    /// same one is never sent and nothing loops. Interrupted bank uploads of
    /// the installed release continue where they stopped, a newer release
    /// replaces them, a busy hub gets one retry 30 s later, and Find
    /// definition reports what the device resolves to; a bank that failed
    /// waits for a manual update (automatic runs of the page skip it). The
    /// Catalog module's code (uploads included) runs in Node against a mock
    /// of the hub's upload rules and a mirror (when Node is installed).
    #[test]
    fn shards_follow_a_newer_catalog_manifest_once() {
        let src = include_str!("../web/ui/catalog.html");
        let (a, b) = (src.find("async function ctPut(").unwrap(), src.find("const ctFetch=").unwrap());
        let b64 = include_str!("../web/index.html").lines().find(|l| l.starts_with("const b64=")).expect("b64 line");
        let signed = include_bytes!("../../hub-app/tests/golden/baseline/catalog/manifest.cose");
        let mock = r#"
const eq=(a,b)=>{a=JSON.stringify(a);b=JSON.stringify(b);if(a!==b)throw new Error(a+" != "+b)};
// A COSE_Sign1 around a canonical manifest map (0: 1, 1: date, 2: serial, ...).
const man=(date,serial)=>{const s=serial<24?[serial]:serial<256?[24,serial]:[25,serial>>8,serial&255];
 return Uint8Array.from([0xD2,0x84,0x43,0xA1,0x01,0x27,0xA1,0x04,0x48,0,1,1,26,0,1,1,26,0x59,0,40,0xA6,0,1,1,26,date>>>24,date>>16&255,date>>8&255,date&255,2,...s,3,0x80,4,0x80,5,0x80,0x58,0x40,...Array(64).fill(0)])};
const V=(d,s)=>d*65536+s,old=V(20261121,1),cur=V(20261124,1),next=V(20261125,1);
eq(ctVer(SIGNED),V(20261121,3));
for(const s of [0,23,24,255,256,65535])eq(ctVer(man(20261124,s)),V(20261124,s));
eq(ctVer(Uint8Array.from([1,2,3])),undefined);
// Bank and shard files of a release: its version in the first 8 bytes.
const file=(v,n)=>{const u=new Uint8Array(n);new DataView(u.buffer).setFloat64(0,v);return u};
const fv=a=>new DataView(Uint8Array.from(a).buffer).getFloat64(0);
// The hub (crates/hub-app/src/catalog/service.rs): one upload at a time;
// offset 0 starts one, refused as busy while another target's advanced in
// the last 30 s, else dropping it; other offsets continue it; a commit
// checks it (manifest anti-rollback, bank and shards against the
// manifest). `has`: the bank of that release holds the device's definition.
let H,M,log,toasts,now=0;
globalThis.setTimeout=(f,ms)=>{now+=ms;f()};
const devs={d:{name:"Remote"}},dL=d=>(d.definition||[]).filter(l=>l.source!="generic"),$=()=>({classList:{add(){},remove(){}}});
const ct0={config:{base_url:"m"}};let ct=ct0;
const ctMsg=()=>{},toast=m=>toasts.push(m),ctSay=toast,ctTxt=m=>"catalog updated to "+m.catalog_date+"."+m.catalog_serial;
const mj=v=>({version:v,catalog_date:Math.floor(v/65536),catalog_serial:v%65536});
const call=async(t,p={})=>{now+=100;if(H.other){H.up={target:"web",bytes:[]};H.at=now}
 if(t=="catalog/status")return{manifest:H.v&&mj(H.v),wants:{bank:H.bank<H.v?{name:"bank.zdb",sha256:"s"+H.v}:null},upload:H.up&&{target:H.up.target,written:H.up.bytes.length},missing:H.def?[]:[{device:"d",shards:H.noshard?[]:[0x44]}],config:ct0.config};
 if(t=="devices/get")return{definition:[{source:"generic",id:"generic"}].concat(H.def?[{source:H.def,id:"ikea/x"}]:[])};
 if(t=="catalog/upload_chunk"){log.push(p.target+"@"+p.offset);
  if(p.offset==0){if(H.up&&H.up.target!=p.target&&now-H.at<30000)throw Error("a "+H.up.target+" upload is in progress");H.up={target:p.target,bytes:[]}}
  if(!H.up||H.up.target!=p.target)throw Error("no "+p.target+" upload in progress (start at offset 0)");
  if(p.offset!=H.up.bytes.length)throw Error("expected offset "+H.up.bytes.length);
  H.at=now;H.up.bytes.push(...Buffer.from(p.data,"base64"));return{written:H.up.bytes.length}}
 if(t=="catalog/commit"){log.push("commit "+p.target);const u=H.up;H.up=null;if(!u||u.target!=p.target)throw Error("no "+p.target+" upload in progress");
  if(p.target=="manifest"){const v=ctVer(Uint8Array.from(u.bytes));if(v<H.v)throw Error("manifest version "+v+" is older than the installed "+H.v);H.v=v;return Object.assign(mj(v),{wants:{bank:H.bank<v?{name:"bank.zdb",sha256:"s"+v}:null}})}
  if(fv(u.bytes)!=H.v)throw Error(p.target=="shard"?"shard 44: SHA-256 differs from the installed manifest":"bank: SHA-256 differs");
  if(p.target=="bank"){H.bank=H.v;if(H.has==H.v)H.def="catalog";return{}}
  if(!H.none)H.def="fetched";return{definitions:H.none?[]:["ikea/x"]}}};
// The mirror: manifests in turn (the last one stays), files of one release.
const ctGet=async n=>{log.push("get "+n);if(n=="bank.zdb"&&M.nobank)throw Error("bank.zdb: HTTP 404");if(n=="manifest.cose"){const v=M.man.length>1?M.man.shift():M.man[0];return man(Math.floor(v/65536),v%65536)}
 return file(M.files,n=="bank.zdb"?5000:100)};
// A new page each time (the failed files it remembers start empty).
const setup=(h,m,f)=>{H=Object.assign({v:old,bank:old},h);M={man:m,files:f};log=[];toasts=[];ctBad.clear()};
const run=async(h,m,f,upd)=>{setup(h,m,f);try{const r=await ctGo(()=>ctMiss("d"),upd);return[r.n,r.m&&r.m.version]}catch(e){return e.message}};
const DIFF="shard 44: SHA-256 differs from the installed manifest",BANK=["get bank.zdb","bank@0","bank@4096","commit bank"],SHARD=["get shard-44.cbor","shard@0","commit shard"];
(async()=>{
 // Newer online: its manifest and bank first, then the shard.
 eq(await run({},[cur],cur,1),[1,cur]);eq(log,["get manifest.cose","manifest@0","commit manifest",...BANK,...SHARD]);eq([H.v,H.bank],[cur,cur]);
 // The mirror's manifest still old at first, its shard new: refused, update, one retry.
 eq(await run({},[old,cur],cur,1),[1,cur]);eq(log,["get manifest.cose",...SHARD,"get manifest.cose","manifest@0","commit manifest",...BANK,...SHARD]);
 eq(toasts,["catalog updated to 20261124.1, retrying…"]);
 // Older online: never sent, the shard error once, no loop.
 eq(await run({v:cur,bank:cur},[old],old,1),DIFF);eq(log,["get manifest.cose",...SHARD,"get manifest.cose"]);eq(H.v,cur);
 // A mirror that keeps moving on: one update and one retry only.
 eq(await run({bank:1e15},[old,cur,next],V(20261126,1),1),DIFF);eq(log,["get manifest.cose",...SHARD,"get manifest.cose","manifest@0","commit manifest",...SHARD]);eq(H.v,cur);
 // The same online: nothing sent but the shard.
 eq(await run({v:cur,bank:cur},[cur],cur,1),[1,0]);eq(log,["get manifest.cose",...SHARD]);
 // Without updates (fetching for new devices, daily updates off): the error.
 eq(await run({},[cur],cur,0),DIFF);eq(log,SHARD);
 // The installed release's bank never arrived (same manifest online): installed.
 eq(await run({v:cur},[cur],cur,1),[1,cur]);eq(log,["get manifest.cose",...BANK,...SHARD]);eq(H.bank,cur);
 // ... interrupted 5 s ago: it continues where it stopped (no busy, no restart).
 setup({v:cur,up:{target:"bank",bytes:Array.from(file(cur,5000).subarray(0,4096))},at:now-5000,has:cur},[cur],cur);
 eq(await ctFind("d"),"catalog updated to 20261124.1; Remote now uses ikea/x");eq(log,["get manifest.cose","get bank.zdb","bank@4096","commit bank"]);
 // The mirror moved on during an interrupted install of the old release:
 // the new manifest replaces it (once idle), its bank from the start.
 setup({v:old,bank:0,up:{target:"bank",bytes:Array(4096).fill(0)},at:now-60000},[cur],cur);
 eq(await ctGo(()=>ctMiss("d"),1).then(r=>[r.n,r.m.version]),[1,cur]);eq(log,["get manifest.cose","manifest@0","commit manifest",...BANK,...SHARD]);
 // ... interrupted 5 s ago: busy, one retry 30 s later.
 setup({v:old,bank:0,up:{target:"bank",bytes:Array(4096).fill(0)},at:now-5000},[cur],cur);
 eq(await ctGo(()=>ctMiss("d"),1).then(r=>[r.n,r.m.version]),[1,cur]);eq(log,["get manifest.cose","manifest@0","manifest@0","commit manifest",...BANK,...SHARD]);
 eq(toasts,["the hub is busy with another catalog upload, trying again in 30 s…"]);
 // A failed update (the mirror lacks the bank) does not stop the shards,
 // which match the manifest it installed.
 setup({},[cur],cur);M.nobank=1;
 eq(await ctGo(()=>ctMiss("d"),1).then(r=>[r.n,r.m]),[1,undefined]);eq(log,["get manifest.cose","manifest@0","commit manifest","get bank.zdb",...SHARD]);
 // ... and with no shard to fetch, the update's error is the answer.
 setup({noshard:1},[cur],cur);M.nobank=1;
 eq(await ctFind("d").catch(e=>e.message),"catalog update: bank.zdb: HTTP 404");
 // Another client keeps uploading: one retry 30 s later each, then its error.
 setup({other:1},[cur],cur);
 eq(await ctGo(()=>ctMiss("d"),1).catch(e=>e.message),"a web upload is in progress");
 eq(log,["get manifest.cose","manifest@0","manifest@0","get shard-44.cbor","shard@0","shard@0"]);
 // A newer bank holding the device's definition: no shard, and Find definition says so.
 setup({has:cur},[cur],cur);
 eq(await ctFind("d"),"catalog updated to 20261124.1; Remote now uses ikea/x");eq(log,["get manifest.cose","manifest@0","commit manifest",...BANK]);
 // A bank that failed (the mirror lacks it) waits for a manual update:
 // automatic runs of the same page skip it (~500 KB each), the shards go on.
 setup({v:cur},[cur],cur);M.nobank=1;
 eq(await ctGo(()=>ctMiss("d"),1,1).then(r=>r.n),1);eq(log,["get manifest.cose","get bank.zdb",...SHARD]);
 delete H.def;log=[];
 eq(await ctGo(()=>ctMiss("d"),1,1).then(r=>r.n),1);eq(log,["get manifest.cose",...SHARD]);
 delete H.def;log=[];M.nobank=0;
 eq(await ctGo(()=>ctMiss("d"),1).then(r=>[r.n,r.m.version]),[1,cur]);eq(log,["get manifest.cose",...BANK,...SHARD]);
 // ... and once installed it is not skipped any more (a later release's bank).
 delete H.def;log=[];M={man:[next],files:next};
 eq(await ctGo(()=>ctMiss("d"),1,1).then(r=>r.n),1);eq(log,["get manifest.cose","manifest@0","commit manifest",...BANK,...SHARD]);
 // Nothing for it anywhere.
 setup({v:cur,bank:cur,none:1},[cur],cur);
 eq(await ctFind("d"),"the catalog has no definition for Remote");eq(log,["get manifest.cose",...SHARD]);
})().catch(e=>{console.error(e);process.exit(1)});
"#;
        let js = format!("{b64}\n{}\nconst SIGNED=Uint8Array.from({signed:?});\n{mock}", &src[a..b]);
        assert!(node(&js, ""), "catalog update checks failed");
    }


    #[test]
    fn basic_page_omits_manual_console_but_keeps_normal_device_controls() {
        let plain = text(PLAIN);
        for advanced in [Features { advanced: true, ota: true, ota_store: true, ..PLAIN }, Features { dev: true, ..PLAIN }] {
            let advanced = text(advanced);
            for item in ["data-tab=\"zcl\"", "zcl/read", "tuya/send", "zFrame(", "onTab.zcl"] {
                assert!(!plain.contains(item), "{item} remains in the basic page");
                assert!(advanced.contains(item), "{item} missing from the advanced page");
            }
        }
        for item in [
            "entities/command",
            "data-tab=\"groups\"",
            "data-tab=\"conv\"",
            "entities/reporting",
            "reporting/defaults/set",
        ] {
            assert!(plain.contains(item), "normal functionality lost: {item}");
        }
        let on = PLAIN.sections();
        assert!(select("// advanced:start\nx\n", &on).is_err());
        assert!(select("// store:start\n// stream:start\n", &on).is_err());
        assert!(select("// stream:end\n", &on).is_err());
        assert!(select("// ota:start\n// ota:start\n// ota:end\n// ota:end\n", &on).is_err());
        assert!(select("// ota:start\n// store:start\n// ota:end\n// store:end\n", &on).is_err());
        // Sections nest: a line is kept when every enclosing section is.
        let nested = "a\n// ota:start\nb\n// stream:start\nc\n// stream:end\n// ota:end\nd\n";
        assert_eq!(select(nested, &on).unwrap(), "a\nd\n");
        let on = Features { ota: true, ..PLAIN }.sections();
        assert_eq!(select(nested, &on).unwrap(), "a\nb\nc\nd\n");
        let on = Features { ota_store: true, ..PLAIN }.sections();
        assert_eq!(select(nested, &on).unwrap(), "a\nb\nd\n");
        // The developer edition implies the console; devmode is its absence.
        let dm = "// devmode:start\nm\n// devmode:end\n// dev:start\nd\n// dev:end\n// advanced:start\na\n// advanced:end\n";
        assert_eq!(select(dm, &PLAIN.sections()).unwrap(), "");
        assert_eq!(select(dm, &Features { advanced: true, ..PLAIN }.sections()).unwrap(), "m\na\n");
        assert_eq!(select(dm, &Features { dev: true, ..PLAIN }.sections()).unwrap(), "d\na\n");
    }

    /// Images without the OTA server (4 MB) have no Updates tab and send
    /// no `ota/*` request.
    #[test]
    fn page_without_ota_has_no_updates_tab() {
        for f in Features::all().into_iter().filter(|f| !f.ota) {
            let page = text(f);
            for item in ["data-tab=\"ota\"", "\"ota/", "\"ota\"", "otRe(", "otEv(", "otNeed(", "id=\"ot"] {
                assert!(!page.contains(item), "{item} in the page without OTA ({f:?})");
            }
            assert!(text(Features { ota: true, ..f }).contains("\"ota\""), "the OTA pages subscribe to the ota topic");
        }
    }

    /// The gateway's cards (storage claim, the hub's own firmware,
    /// Ethernet) only in the gateway's page.
    #[test]
    fn gateway_cards_only_in_the_gateway_page() {
        let items = ["\"storage/claim\"", "\"firmware/begin\"", "\"firmware/chunk\"", "replace_stock", "s.ethernet", "id=\"gsb\"", "id=\"gsban\"", "gwBan("];
        for f in Features::all() {
            let page = text(f);
            for item in items {
                assert_eq!(page.contains(item), f.gateway, "{item}: {f:?}");
            }
        }
    }

    /// The Updates tab offers the image source the firmware has: the store
    /// (upload) or this browser tab (streaming).
    #[test]
    fn updates_tab_follows_the_image_store() {
        for f in Features::all().into_iter().filter(|f| f.ota) {
            let page = text(f);
            for item in ["ota/upload/begin", "id=\"otb\""] {
                assert_eq!(page.contains(item), f.ota_store, "{item}");
            }
            for item in ["ota/stream/offer", "id=\"otsb\"", "beforeunload"] {
                assert_eq!(page.contains(item), !f.ota_store, "{item}");
            }
            for item in ["data-tab=\"ota\"", "ota/start", "ota/block_data", "function otRe("] {
                assert!(page.contains(item), "{item}");
            }
        }
    }

    #[test]
    fn minifier_keeps_code_and_html() {
        let html = "<p>\n  <b>x</b>\n</p>\n<style>\n a{b:c}\n\n d,\n e{f:g}\n</style>\n<script>\n// c\n let a=1,\n  b=\"//\";\n f(a,\n b)\n x=1 // y,\n z()\n</script>\n";
        let want = "<p>\n  <b>x</b>\n</p>\n<style>\na{b:c}d,e{f:g}\n</style>\n<script>\nlet a=1,b=\"//\";\nf(a,b)\nx=1 // y,\nz()\n</script>\n";
        assert_eq!(super::minify::minify(html).unwrap(), want);
        assert!(super::minify::minify("<script>\nlet s=`a\nb`\n</script>\n").is_err());
        assert!(super::minify::minify("<style>\na{b:\nc}\n</style>\n").is_err());
        assert!(super::minify::minify("<script>\nx\n").is_err());
    }

    #[test]
    fn page_is_self_contained_and_uses_only_the_api() {
        for f in Features::all() {
            let b = build(f);
            // Device pictures are the one optional outside resource (an
            // <img>, switched off in the page's settings): Zigbee2MQTT's
            // image folder. The fallback icon's SVG namespace is a name,
            // not a fetch.
            let page = b
                .page
                .replacen("\"https://www.zigbee2mqtt.io/images/devices/\"", "", 1)
                .replace("xmlns=\"http://www.w3.org/2000/svg\"", "");
            let mut all = page.clone();
            for m in &b.modules {
                all.push_str(&m.js);
            }
            for external in ["http://", "https://", "src=\"//", "<link", "<script src"] {
                assert!(!all.contains(external), "{f:?}: external reference {external:?}");
            }
            // Dynamic imports only of the hub's own web tools (`/zdefc/`,
            // served from hubdata); fetches only of this build's modules
            // and, in the Catalog, of the catalog mirror the user set.
            assert!(all.match_indices("import(").all(|(i, _)| all[i..].starts_with("import(\"/zdefc/")));
            assert!(page.match_indices("fetch(").all(|(i, _)| page[i..].starts_with("fetch(\"/ui/\"+UIB+\"/\"+n+\".js\")")));
            for m in &b.modules {
                let ok = |i: usize| m.name == "catalog" && m.js[i..].starts_with("fetch(ct.config.base_url+");
                assert!(m.js.match_indices("fetch(").all(|(i, _)| ok(i)), "{}: fetch", m.name);
            }
            assert!(page.contains("+\"/api\""));
        }
    }

    #[test]
    fn etag_and_conditional_requests() {
        assert!(ETAG.starts_with('"') && ETAG.ends_with('"') && ETAG.len() == 18);
        assert!(not_modified(ETAG));
        assert!(not_modified(&std::format!("\"x\", W/{ETAG}")));
        assert!(!not_modified("\"0000000000000000\""));
        assert!(MODULES.iter().all(|m| m.etag != ETAG && !not_modified(m.etag) && matches(&std::format!("W/{}", m.etag), m.etag)));
    }
}

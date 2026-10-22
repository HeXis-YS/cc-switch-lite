#!/usr/bin/env python3
"""Regenerate independent protocol oracle from the pinned upstream git object.
Usage: python3 scripts/reference-vendor.py /path/to/cc-switch
Outputs reference/cc-switch-reference/src and provenance.json; requires git/Python.
Never reads production src/. Review generated changes before updating the pin.
"""
import hashlib
import json
import re
from pathlib import Path
import subprocess
import sys

PIN = '214e75ba7f625c679dd4e61df166ec4463245fe7'
ROOT = Path(__file__).resolve().parents[1] / 'reference/cc-switch-reference'
UPSTREAM = Path(sys.argv[1]).resolve()
records = []

def read(path):
    return subprocess.check_output(['git', '-C', str(UPSTREAM), 'show', f'{PIN}:src-tauri/src/{path}']).decode()

def save(path, text, source, mode='verbatim'):
    target = ROOT / 'src' / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text)
    records.append(dict(target=path, upstream=source, adaptation=mode,
                        sha256=hashlib.sha256(text.encode()).hexdigest()))

providers = ['codex_chat_common', 'codex_chat_history', 'codex_chat_tool_history',
             'codex_compaction', 'codex_responses_sse', 'inline_think',
             'streaming_codex_anthropic', 'streaming_codex_chat', 'transform',
             'transform_codex_anthropic', 'transform_codex_chat', 'transform_responses',
             'reasoning_bridge', 'transform_codex_chat_moonshot_schema',
             'transform_codex_responses_namespace', 'transform_codex_responses_xai_sanitize']
for name in providers:
    path = f'proxy/providers/{name}.rs'
    save(path, read(path), path)
for name in ['json_canonical', 'sse', 'thinking_optimizer', 'tool_media', 'cache_injector', 'body_filter', 'session']:
    path = f'proxy/{name}.rs'
    save(path, read(path), path)
source = read('provider.rs')
start = source.index('/// Codex Responses -> Chat Completions')
end = source.index('/// Local proxy request overrides', start)
save('provider.rs', 'use serde::{Serialize, Deserialize};\n' + source[start:end], 'provider.rs', 'exact reasoning config declaration; serde imports only')
source = read('proxy/types.rs')
start = source.index('/// 请求优化器配置')
end = source.index('/// Copilot 优化器配置', start)
save('proxy/types.rs', 'use serde::{Serialize, Deserialize};\nfn default_true() -> bool { true }\n' + source[start:end], 'proxy/types.rs', 'exact optimizer config and default; serde imports and default_true helper')
(ROOT / 'src/proxy/providers/mod.rs').write_text(''.join(f'pub mod {n};\n' for n in providers) + '''\n/// Exact endpoint predicate from upstream proxy/providers/codex.rs.\npub fn is_codex_responses_endpoint(endpoint: &str) -> bool {\n    let path = endpoint.split_once('?').map_or(endpoint, |(path, _query)| path);\n    matches!(path, \"/responses\" | \"/v1/responses\" | \"/responses/compact\" | \"/v1/responses/compact\")\n}\n''')
(ROOT / 'src/proxy/mod.rs').write_text(''.join(f'pub mod {n};\n' for n in ['error', 'providers', 'types', 'json_canonical', 'sse', 'thinking_optimizer', 'tool_media', 'cache_injector', 'body_filter', 'session']))

forwarder = read('proxy/forwarder.rs')
codex = read('proxy/providers/codex.rs')
def function(source, name):
    match = re.search(r'^(?:pub(?:\([^)]*\))? )?fn ' + name + r'\b.*?^}', source, flags=re.M|re.S)
    if not match:
        raise RuntimeError('Missing pinned helper ' + name)
    return match.group(0)
helpers = ['base_url_is_full_endpoint', 'is_codex_client_fingerprint_header',
           'should_strip_codex_client_fingerprint_header', 'is_streaming_request',
           'is_opencode_upstream', 'native_session_header', 'pick_opencode_session',
           'conversation_fingerprint', 'without_cache_control']
start = forwarder.index('        let mut ordered_headers = http::HeaderMap::new();')
end = forwarder.index('        // Codex OAuth 反代', start)
block = forwarder[start:end]
url_method = re.search(r'    fn build_url\(&self,.*?^    }', codex, flags=re.M|re.S).group(0)
url_function = url_method.replace('fn build_url(&self, ', 'fn build_url(')
header_oracle = (ROOT / 'adapter/header_prelude.rs').read_text() + block + "\n    if !ordered_headers.contains_key(http::header::CONTENT_TYPE) { ordered_headers.insert(http::header::CONTENT_TYPE, HeaderValue::from_static(\"application/json\")); }\n    (url, ordered_headers)\n}\n" + '\n\n'.join(function(forwarder,n) for n in helpers) + '\n' + function(codex,'is_origin_only_url') + '\n' + url_function
header_oracle = header_oracle.replace('super::providers::is_codex_responses_endpoint', 'crate::proxy::providers::is_codex_responses_endpoint')
save('header_oracle.rs', header_oracle, 'proxy/forwarder.rs; proxy/providers/codex.rs', 'verbatim header loop/helpers and URL builder; fixture prelude; method receiver removed')

(ROOT / 'provenance.json').write_text(json.dumps(dict(commit=PIN, files=records), indent=2) + '\n')

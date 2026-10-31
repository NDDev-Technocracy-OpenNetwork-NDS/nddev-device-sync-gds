#!/usr/bin/env python3
"""Owned browser HTTP negatives only; no GitHub/owner credential fixture."""
import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--runner', type=Path, required=True)
parser.add_argument('--wasm', type=Path, required=True)
parser.add_argument('--playwright-module', type=Path, required=True)
args = parser.parse_args()
observed = []

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_args): pass
    def cors(self):
        self.send_header('Access-Control-Allow-Origin', '*')
        self.send_header('Access-Control-Allow-Headers', 'Authorization, X-GitHub-Api-Version')
        self.send_header('Access-Control-Allow-Methods', 'GET, OPTIONS')
        self.send_header('Access-Control-Expose-Headers', 'Link, Retry-After, X-RateLimit-Remaining')
    def do_OPTIONS(self):
        self.send_response(204); self.cors(); self.end_headers()
    def do_GET(self):
        case = self.path.split('/')[1]
        valid = self.headers.get('Authorization') == 'Bearer generated_fixture_invalid'
        valid &= self.headers.get('X-GitHub-Api-Version') == '2026-03-10'
        valid &= not self.headers.get('Cookie') and not self.headers.get('Signature')
        observed.append({'case': case, 'headers_valid': bool(valid)})
        if not valid:
            self.send_response(400); self.cors(); self.end_headers(); return
        code, body = 200, b'not json'
        if case == 'oversized': body = b' ' * (1048576 + 1)
        if case == 'rate': code, body = 429, b''
        if case == 'denied': code, body = 401, b''
        if case == 'redirect': code, body = 302, b''
        if case == 'slow': time.sleep(20)
        self.send_response(code); self.cors()
        if case == 'rate': self.send_header('Retry-After', '10')
        if case == 'redirect': self.send_header('Location', 'https://other.invalid/')
        self.send_header('Content-Length', str(len(body))); self.end_headers()
        try: self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError): pass

fixture = ThreadingHTTPServer(('127.0.0.1', 19483), Handler)
threading.Thread(target=fixture.serve_forever, daemon=True).start()
with tempfile.TemporaryDirectory(prefix='nds-github-browser-') as temporary:
    log = Path(temporary) / 'runner.log'
    with log.open('w') as output:
        runner = subprocess.Popen([str(args.runner.resolve()), str(args.wasm.resolve()), '--nocapture'],
            env={**os.environ, 'NO_HEADLESS':'1', 'WASM_BINDGEN_TEST_ADDRESS':'127.0.0.1:19484'},
            stdin=subprocess.DEVNULL, stdout=output, stderr=subprocess.STDOUT)
    try:
        deadline = time.monotonic() + 30
        while 'Interactive browsers tests' not in log.read_text():
            if runner.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError('owned WASM browser runner failed to start: ' + log.read_text()[-2000:])
            time.sleep(.05)
        driver = Path(temporary) / 'browser.cjs'
        driver.write_text("""
const {chromium}=require(process.argv[2]);
(async()=>{const browser=await chromium.launch({headless:true,executablePath:'/usr/bin/google-chrome',args:['--no-sandbox']});
try {const page=await browser.newPage();await page.goto('http://127.0.0.1:19484');
await page.waitForFunction(()=>document.body.innerText.includes('test result:'),null,{timeout:90000});
const text=await page.locator('body').innerText();console.log(text);
if(!text.includes('test result: ok. 2 passed; 0 failed'))process.exitCode=1;
}finally{await browser.close();}})().catch(()=>{process.exitCode=1;});
""")
        browser = subprocess.run(['node', str(driver), str(args.playwright_module.resolve())],
            text=True, capture_output=True, timeout=120)
        print(browser.stdout)
        if browser.returncode: raise RuntimeError('owned browser tests failed: ' + log.read_text()[-2000:])
        assert all(item['headers_valid'] for item in observed)
        assert {'malformed','oversized','rate','redirect','denied','slow'} <= {item['case'] for item in observed}
        print(json.dumps({'fixture':'controlled owned browser HTTP negatives','provider_positive':'unissued',
            'cases':sorted({item['case'] for item in observed}),'private_headers_or_tokens_logged':False}))
    finally:
        runner.terminate()
        try: runner.wait(timeout=3)
        except subprocess.TimeoutExpired: runner.kill(); runner.wait(timeout=3)
        fixture.shutdown(); fixture.server_close()

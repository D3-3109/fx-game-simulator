// node tests/chart.js < static/app.js
const assert = require('node:assert/strict');
const vm = require('node:vm');
const script = require('node:fs').readFileSync(0, 'utf8');
const elements = {};
let domWrites=0;
const frames=new Map();let frameId=0;
function mockNode(tag='div',id=''){
 const node={tag,id,children:[],parentElement:null,style:{setProperty(key,value){this[key]=value;domWrites++;}},dataset:{},hidden:false,value:'',className:'',
  clientHeight:id==='workspace'?600:400,clientWidth:1000,offsetHeight:10,scrollHeight:0,scrollTop:0,attributes:{},options:[],listeners:{},html:'',text:'',htmlWrites:0,
  addEventListener(type,fn){(this.listeners[type]||=[]).push(fn);},reportValidity(){return true;},
  setAttribute(key,value){this.attributes[key]=String(value);domWrites++;},removeAttribute(key){delete this.attributes[key];domWrites++;},
  setPointerCapture(id){this.capture=id;},add(option){this.options.push(option);},
  append(...children){for(const child of children)this.insertBefore(child,null);},
  insertBefore(child,before){if(child.parentElement)child.remove();const i=before?this.children.indexOf(before):-1;this.children.splice(i<0?this.children.length:i,0,child);child.parentElement=this;this.html='';domWrites++;},
  remove(){if(this.parentElement){const a=this.parentElement.children;a.splice(a.indexOf(this),1);this.parentElement=null;domWrites++;}},
  replaceChildren(...children){this.children.forEach(n=>n.parentElement=null);this.children=[];this.options=[];this.html='';this.text='';domWrites++;this.append(...children);},
  closest(){for(let n=this;n;n=n.parentElement)if(n.hidden)return n;return null;},
  querySelector(){return this;},
 };
 Object.defineProperties(node,{
  textContent:{get(){return this.text;},set(v){this.text=v;this.html='';domWrites++;}},
  innerHTML:{get(){return this.html||this.children.map(n=>n.outerHTML).join('')||String(this.text);},set(v){this.html=v;this.children=[];this.text='';this.htmlWrites++;domWrites++;}},
  outerHTML:{get(){return `<${this.tag}${this.className?' class="'+this.className+'"':''}>${this.innerHTML}</${this.tag}>`;}}
 });return node;
}
const panels={};
const documentMock={hidden:false,querySelectorAll(){return [];},querySelector(selector){return panels[selector]??null;},
 getElementById(id){return elements[id]||=mockNode('div',id);},createElement:tag=>mockNode(tag),addEventListener(){},head:{append(){}},title:''};
function flushFrames(){const pending=[...frames.values()];frames.clear();for(const fn of pending)fn(now);}

let autoScale = false, sets = 0, updates = [], removed = [], ranges = [];
let response = [], displayed = [], now = Date.now(), requests = [], responseStamp = 0;
const series = {
  applyOptions(options) { this.options=options; },
  setData(data) { sets++; displayed = data.map(bar => ({...bar})); },
  update(bar) {
    const latest = displayed.at(-1);
    if(latest && bar.time < latest.time) throw Error('Cannot update older candle');
    if(latest && bar.time === latest.time) displayed[displayed.length-1] = {...bar};
    else displayed.push({...bar});
    updates.push(bar);
  },
  removePriceLine(line) { removed.push(line); },
  createPriceLine(options) { return {...options,changes:0,applyOptions(next){Object.assign(this,next);this.changes++;}}; },
};
const scale = { applyOptions(options) { autoScale = options.autoScale; } };
const chart = {
  priceScale(id) { assert.equal(id, 'right'); return scale; },
  timeScale() { return {setVisibleLogicalRange(range) { ranges.push(range); }}; },
};
const context = vm.createContext({
  document:documentMock,
  localStorage: {},
  location: {host:'localhost:8000',reload(){this.reloaded=true;}},
  WebSocket: class {constructor(){this.sent=[];}send(text){this.sent.push(JSON.parse(text));}close(){this.closed=true;this.readyState=3;}},
  AbortSignal,AbortController,
  requestAnimationFrame(callback){frames.set(++frameId,callback);return frameId;},cancelAnimationFrame(id){frames.delete(id);},
  setInterval(){},ResizeObserver:class {observe(){}},
  Date: class extends Date {static now(){return now;} },
  Option: class {constructor(text,value){this.text=text;this.value=value??text;} },
  fetch: async url => { requests.push(url); return {json: async () => ({ok: true, klines: response, updated_at: responseStamp})}; },
  setTimeout() {}, clearTimeout() {}, confirm() { return false; },
  console,
});
vm.runInContext(script, context);
Object.assign(context, {testSeries: series, testChart: chart});
vm.runInContext('series=testSeries;chart=testChart;', context);
// Most assertions below compare raw UTC bar times; the Beijing display shift has its own test.
assert.equal(vm.runInContext('CHART_SHIFT', context),8*3600,'chart axis must read Beijing time');
vm.runInContext('CHART_SHIFT=0', context);
const raw = code => vm.runInContext(code, context);
const run = code => {const result=raw(code);flushFrames();return result?.then?result.then(value=>{flushFrames();return value;}):result;};
const candle = price => {
  const time = Math.floor(Date.now() / 1000 / 300) * 300;
  return [[(time-300)*1000,price,price+1,price-1,price],
          [time*1000,price,price+1,price-1,price]];
};

(async () => {
  // Fixed v5 CDN, matching local/SRI fallback, bounded waits, deduplicated loads and retry.
  const makeLoader=()=>{
    const scripts=[],timers=new Map(),loaderElements={};let clock=0;
    const sandbox=vm.createContext({...context,LightweightCharts:undefined,localStorage:{},
      document:{...documentMock,querySelectorAll() { return []; },getElementById(id){return loaderElements[id]||={...context.document.getElementById(id),
          style:{setProperty(key,value){this[key]=value;}},attributes:{},dataset:{},options:[],listeners:{},value:''};},
        head:{append(node){scripts.push(node);}},
        createElement(tag){assert.equal(tag,'script');return {remove(){this.removed=true;}};}},
      setTimeout(callback,ms){assert.equal(ms,5000);timers.set(++clock,callback);return clock;},
      clearTimeout(id){timers.delete(id);},
    });
    vm.runInContext(script,sandbox);
    return {sandbox,scripts,timers,load:()=>vm.runInContext('loadChartLibrary()',sandbox),
      ready(){sandbox.LightweightCharts={version:()=> '5.2.1',CandlestickSeries:{type:'Candlestick'}};}};
  };
  const tick=()=>new Promise(resolve=>setImmediate(()=>{flushFrames();resolve();}));
  const cdn=makeLoader(),cdnLoad=cdn.load();
  assert.equal(cdn.load(),cdnLoad,'parallel chart requests downloaded library twice');
  assert.equal(cdn.scripts.length,1);
  assert.equal(cdn.scripts[0].src,'/assets/lightweight-charts-5.2.1.js');
  assert.equal(cdn.scripts[0].crossOrigin,'anonymous');
  assert.equal(cdn.scripts[0].integrity,'sha256-4hzFyqAibvML2FScULnvkmYV8qTua05IY1NHelX1mM8=');
  cdn.ready();cdn.scripts[0].onload();await cdnLoad;
  assert.equal(cdn.timers.size,0);await cdn.load();assert.equal(cdn.scripts.length,1);
  const fallback=makeLoader(),fallbackLoad=fallback.load();
  fallback.scripts[0].onerror();await tick();
  assert.equal(fallback.scripts[0].removed,true);
  assert.equal(fallback.scripts[1].src,'https://unpkg.com/lightweight-charts@5.2.1/dist/lightweight-charts.standalone.production.js');
  assert.equal(fallback.scripts[1].integrity,fallback.scripts[0].integrity);
  fallback.ready();fallback.scripts[1].onload();await fallbackLoad;
  const timeout=makeLoader(),timeoutLoad=timeout.load();
  timeout.timers.values().next().value();await tick();
  assert.equal(timeout.scripts[0].removed,true);assert.equal(timeout.scripts.length,2);
  timeout.ready();timeout.scripts[1].onload();await timeoutLoad;
  const wrong=makeLoader(),wrongLoad=wrong.load();
  wrong.sandbox.LightweightCharts={version:()=> '4.2.3'};
  wrong.scripts[0].onload();await tick();assert.equal(wrong.scripts.length,2,'v4 accepted as v5');
  wrong.ready();wrong.scripts[1].onload();await wrongLoad;
  const retry=makeLoader(),broken=retry.load();
  const rejected=assert.rejects(broken,/加载超时/);
  retry.scripts[0].onerror();await tick();retry.timers.values().next().value();await rejected;
  const recovered=retry.load();assert.equal(retry.scripts.length,3,'failed library load stayed cached');
  retry.ready();retry.scripts[2].onload();await recovered;

  // First selection, request and live aggregation must agree before any change event.
  assert.equal(run('cur.itv'), '15m', 'default K-line state is not 15m');
  assert.equal(run('cur.sym'), 'NSTAR');
  assert.match(elements['itv-quick'].innerHTML,/aria-pressed="true" onclick="pickItv\('15m'\)"/,'initial interval button disagrees with default');
  assert.equal(run('symSel.value'), run('cur.sym'));
  assert.deepEqual(elements.sym.options.map(option=>option.value),['NSTAR'],'startup exposed legacy pairs');
  assert.equal(elements.sym.disabled,true,'NSTAR display offered a symbol switch');
  assert.equal(typeof elements.sym.onchange,'undefined','legacy symbol switch handler is still installed');
  assert.deepEqual([...elements['itv-quick'].innerHTML.matchAll(/pickItv\('([^']+)'\)/g)].map(m=>m[1]),['1s','1m','5m','15m','30m','1h','4h','1d','1w']);
  assert.doesNotMatch(require('node:fs').readFileSync('static/index.html','utf8'),/id="itv"/,'duplicate interval dropdown is back');
  assert.match(require('node:fs').readFileSync('static/index.html','utf8'),/href="https:\/\/www\.tradingview\.com\/"/,'hidden logo needs the NOTICE attribution link on the login page');
  assert.doesNotMatch(require('node:fs').readFileSync('static/index.html','utf8'),/chart-credit/,'credit is back in the chart toolbar');
  response = candle(100);
  await run('loadK()');
  assert.match(requests[0], /symbol=NSTAR&interval=15m(?:&|$)/, 'initial request used wrong symbol/timeframe');
  assert.equal(run('lastItv'), '15m');
  sets = 0; updates = []; ranges = [];
  assert.equal(elements['chartpane-status'].hidden, true, 'chart loader stayed after HTTP candles');
  assert.equal(elements.chartpane.attributes['aria-busy'], 'false');

  // New NSTAR-only HTTP settings and older mixed WS settings expose the same single pair.
  assert.equal(elements['trade-buy'].disabled,true,'trade enabled before whitelist loaded');
  const originalFetch=context.fetch;
  context.fetch=async url=>{assert.equal(url,'/api/symbols');return {json:async()=>({ok:true,version:8,rows:[{symbol:'NSTAR',enabled:true}]})};};
  await run('loadSymbols()');context.fetch=originalFetch;
  assert.deepEqual([...run('tradableSymbols')],['NSTAR'],'NSTAR-only API did not load');
  assert.equal(elements['trade-buy'].disabled,true,'trade enabled before any quote');
  run("renderQuote(100,'NSTAR',99.97,100.03,true)");
  assert.equal(elements['trade-buy'].disabled,false);assert.equal(elements['spot-submit'].disabled,false);
  context.symbolSettings={version:10,rows:[{symbol:'USDJPY',enabled:true},{symbol:'NSTAR',enabled:true},{symbol:'BTCUSDT',enabled:true},{symbol:'FXSIM',enabled:true}]};
  context.symbolPacket=JSON.stringify({type:'symbols',...context.symbolSettings});
  run('wsConnect();ws.onmessage({data:symbolPacket})');
  assert.deepEqual([...run('tradableSymbols')],['NSTAR'],'mixed API enabled legacy trading');
  assert.deepEqual(elements.sym.options.map(option=>option.value),['NSTAR'],'mixed API exposed legacy choices');
  assert.equal(elements.sym.disabled,true);
  context.symbolSettings={version:11,rows:[{symbol:'NSTAR',enabled:false},{symbol:'FXSIM',enabled:true}]};
  context.symbolPacket=JSON.stringify({type:'symbols',...context.symbolSettings});
  run('ws.onmessage({data:symbolPacket})');
  assert.equal(elements['trade-buy'].disabled,true);assert.equal(elements['trade-sell'].disabled,true);
  assert.equal(elements['spot-submit'].disabled,true);
  assert.equal(run('cur.sym'),'NSTAR','disable silently changed selected pair');
  assert.deepEqual(elements.sym.options.map(option=>option.value),['NSTAR']);
  const disabledOption=elements.sym.options[0];
  assert.equal(disabledOption.disabled,true);assert.match(disabledOption.text,/仅查看/);
  run('applySymbols({version:9,rows:[{symbol:"NSTAR",enabled:true}]})');
  assert.equal(elements['trade-buy'].disabled,true,'stale configuration re-enabled trading');
  const beforeOrders=requests.length;await run("trade('buy')");
  assert.equal(requests.length,beforeOrders,'disabled order still submitted');
  let resolveSymbols;
  context.fetch=()=>new Promise(resolve=>{resolveSymbols=resolve;});
  const loadingSymbols=run('loadSymbols()');
  run('applySymbols({version:12,rows:[{symbol:"FXSIM",enabled:true}]})');
  assert.deepEqual([...run('tradableSymbols')],[],'legacy-only settings enabled a hidden pair');
  resolveSymbols({json:async()=>({ok:true,version:10,rows:[{symbol:'NSTAR',enabled:true}]})});await loadingSymbols;
  assert.equal(elements['trade-buy'].disabled,true,'late HTTP enabled disabled symbol');
  run('applySymbols({version:13,rows:[]})');
  assert.equal(elements.sym.disabled,true);assert.match(elements['symbol-status'].textContent,/暂无启用/);
  assert.equal(elements.sym.value,'NSTAR');assert.equal(elements.sym.options.length,1);
  context.fetch=originalFetch;

  // Account state may contain legacy records, but must never replace the production pair/query.
  const startupState={ok:true,symbol:'NSTAR',price:100,bid:99.97,ask:100.03,quote_valid:true,balance:10000,equity:10000,used:0,leverage:10,positions:[]};
  context.startupState=startupState;
  const startupRequests=[];
  context.fetch=async url=>{startupRequests.push(url);assert.equal(url,'/api/state');return {json:async()=>startupState};};
  await run('refresh()');
  assert.equal(elements['positions-status'].hidden,true,'NSTAR-only account did not load');
  startupState.symbol='USDJPY';startupState.price=150;
  await run('refresh()');
  assert.equal(run('cur.sym'),'NSTAR','legacy account replaced NSTAR default');
  assert.equal(elements.sym.value,'NSTAR');
  assert.deepEqual(startupRequests,['/api/state','/api/state'],'state refresh requested legacy chart data');
  run('ws.onmessage({data:JSON.stringify({type:"state",...startupState})})');
  assert.equal(run('cur.sym'),'NSTAR','legacy WS state replaced NSTAR default');
  context.fetch=originalFetch;
  run("setMode('spot')");assert.equal(elements['spot-form'].hidden,false);assert.equal(elements['perp-form'].hidden,true);
  run("setMode('perp')");assert.equal(elements['spot-form'].hidden,true);assert.equal(elements['perp-form'].hidden,false);
  assert.equal(run('cur.sym'),'NSTAR');
  const beforeInterval=requests.length;run("pickItv('1s')");await tick();
  assert.match(requests[beforeInterval],/symbol=NSTAR&interval=1s(?:&|$)/);
  assert.equal(run('cur.itv'),'1s');
  sets=0;updates=[];ranges=[];

  // Incoming data cannot alter old chart while new history is loading.
  run("cur={sym:'BTCUSDT',itv:'5m'};kLoaded=false;");
  run('updateLast(60000)');
  assert.equal(updates.length, 0, 'tick entered old chart during switch');
  response = candle(60000);
  await run('loadK()');
  assert.equal(autoScale, true, 'switch did not reset manually scaled price axis');
  assert.equal(sets, 1);
  assert.equal(ranges.length, 1);
  assert.equal(run('lastSym'), 'BTCUSDT');

  // Late state from previous symbol must never push EUR prices into BTC bars.
  updates = [];
  run("render({symbol:'EURUSD',price:1.1,balance:10000,equity:10000,used:0,leverage:100,margin_level:null,positions:[]})");
  assert.equal(updates.length, 0, 'old symbol state contaminated new candle');
  run("render({symbol:'BTCUSDT',price:60001,balance:10000,equity:10000,used:0,leverage:100,margin_level:null,positions:[]})");
  assert.equal(updates.at(-1).close, 60001);

  // Periodic refresh must not erase user's intentional manual zoom/pan.
  autoScale = false;
  await run('loadK()');
  assert.equal(autoScale, false, 'periodic refresh reset manual price zoom');
  assert.equal(sets, 1);
  assert.equal(ranges.length, 1);

  // Timeframe changes reset scale too; same-symbol early ticks stay blocked.
  run("cur.itv='15m';kLoaded=false;");
  updates = [];
  run('updateLast(60002)');
  assert.equal(updates.length, 0);
  await run('loadK()');
  assert.equal(autoScale, true);
  assert.equal(sets, 2);
  assert.equal(ranges.length, 2);

  // Loading active OHLC must not collapse original open/high/low into close.
  now = 1800000000000;
  const t = now / 1000;
  run("cur={sym:'EURUSD',itv:'5m'};lastBar=null;kLoaded=false;");
  response = [[(t-300)*1000,100,120,80,110],[t*1000,100,120,80,110]];
  await run('loadK()');
  const activeBar = () => JSON.parse(run('JSON.stringify(lastBar)'));
  assert.deepEqual(activeBar(), {time:t,open:100,high:120,low:80,close:110}, 'history OHLC lost on load');
  assert.deepEqual(displayed.at(-1), activeBar(), 'displayed current wick lost');
  response[1][4] = 112;
  await run('loadK()');
  assert.equal(activeBar().close, 112, 'history-only refresh froze close price');
  run('updateLast(115)');
  assert.deepEqual(activeBar(), {time:t,open:100,high:120,low:80,close:115}, 'first tick erased historical wicks');

  // A stale poll must preserve tick extremes, opening price and latest close.
  run('updateLast(125);updateLast(75);updateLast(110)');
  response[1] = [t*1000,101,121,79,105];
  await run('loadK()');
  assert.deepEqual(activeBar(), {time:t,open:100,high:125,low:75,close:110}, 'poll reset observed wick');
  assert.deepEqual(displayed.at(-1), activeBar());
  response[1] = [t*1000,100,130,70,105];
  await run('loadK()');
  assert.deepEqual(activeBar(), {time:t,open:100,high:130,low:70,close:110}, 'server extremes not merged');

  // Poll lagging behind next live candle cannot rewind its OHLC.
  now += 300000;
  run('updateLast(111);updateLast(113);updateLast(110)');
  const next = {time:t+300,open:111,high:113,low:110,close:110};
  assert.deepEqual(activeBar(), next, 'new candle inherited previous extremes');
  await run('loadK()');
  assert.deepEqual(activeBar(), next, 'lagging history replaced new live candle');
  assert.deepEqual(displayed.at(-1), next);

  // Historical close alone must not manufacture a flat candle at current time.
  run("cur={sym:'GBPUSD',itv:'5m'};kLoaded=false;");
  response = [[(t-600)*1000,1.2,1.3,1.1,1.25]];
  await run('loadK()');
  assert.deepEqual(activeBar(), {time:t-600,open:1.2,high:1.3,low:1.1,close:1.25});
  assert.deepEqual(displayed.at(-1), activeBar());
  assert.equal(displayed.length, 1, 'history close created fake current candle');

  // FXSIM must use authoritative OHLC, including unobserved intrasecond spikes.
  run("cur={sym:'FXSIM',itv:'1m'};kLoaded=false;");
  now = t * 1000;
  response = [[t*1000,1.1,1.1,1.1,1.1]]; responseStamp=now;
  await run('loadK()');
  run('updateLast(1.09)');
  assert.equal(activeBar().low, 1.1, 'sampled account quote guessed simulated OHLC');
  run('wsConnect()');
  const push = (rows, stamp) => {
    context.packetJSON = JSON.stringify({sym:'FXSIM',price:rows.at(-1)[4],time:stamp,klines:{'1m':rows}});
    run('ws.onmessage({data:packetJSON})');
  };
  const dipped = [t*1000,1.1,1.1,.9,1.09];
  push([dipped], now+400);
  assert.deepEqual(activeBar(), {time:t,open:1.1,high:1.1,low:.9,close:1.09});
  assert.deepEqual(displayed.at(-1), activeBar());
  response = [[t*1000,1.1,1.1,1.1,1.1]]; responseStamp=now;
  await run('loadK()');
  assert.equal(activeBar().low, .9, 'slow pre-spike poll erased wick');
  const nextSim=[(t+60)*1000,1.11,1.11,1.11,1.11];
  // Final previous candle may contain lower low never delivered separately.
  const final=[t*1000,1.1,1.1,.85,1.09];
  push([final,nextSim], now+60100);
  assert.deepEqual(displayed[0], {time:t,open:1.1,high:1.1,low:.85,close:1.09}, 'closed candle lost final low');
  const frozen = JSON.stringify(displayed[0]);
  response=[final,nextSim];responseStamp=now+60100;
  autoScale=false;await run('loadK()');
  assert.equal(JSON.stringify(displayed[0]), frozen);
  assert.equal(autoScale, false, 'simulated tick/poll reset manual price scale');
  responseStamp=now+62000;
  run('kLoaded=false;simBars=[];simStamp=0;lastBar=null;'); // Ctrl+R: discard browser cache.
  await run('loadK()');
  assert.equal(JSON.stringify(displayed[0]), frozen, 'Ctrl+R changed closed candle/wick');
  run("cur.itv='5m';kLoaded=false;");
  push([final,nextSim], now+63000);
  assert.equal(run('simStamp'), now+62000, 'old interval OHLC contaminated switched chart');

  // Loading must reflect actual arrival, independent of WS connection opening.
  const normalFetch=context.fetch;
  run("cur={sym:'BTCUSDT',itv:'1m'};kLoaded=false;");
  let finish;
  context.fetch=()=>new Promise(resolve=>{finish=resolve;});
  const pending=run('loadK()');
  assert.equal(elements['chartpane-status'].hidden,false);
  assert.equal(elements['chartpane-status'].dataset.state,'loading');
  assert.equal(elements['chartpane-status'].textContent,'Loading...');
  assert.equal(elements.chartpane.attributes['aria-busy'],'true');
  finish({json:async()=>({ok:true,klines:candle(60000)})});await pending;
  assert.equal(elements['chartpane-status'].hidden,true);
  const background=run('loadK()');
  assert.equal(elements['chartpane-status'].hidden,true,'periodic refresh covered existing candles');
  finish({json:async()=>({ok:true,klines:candle(60000)})});await background;

  context.fetch=async()=>{throw Error('offline')};
  run('kLoaded=false;');await run('loadK()');
  assert.equal(elements['chartpane-status'].dataset.state,'error','failed load spins forever');
  assert.equal(elements['chartpane-status'].hidden,false);
  assert.equal(elements.chartpane.attributes['aria-busy'],'false');
  context.fetch=normalFetch;response=candle(60000);await run('loadK()');
  assert.equal(elements['chartpane-status'].hidden,true,'successful retry did not clear error');
  // Old request errors cannot replace new chart's loading/completed status.
  let rejectOld;
  context.fetch=()=>new Promise((resolve,reject)=>{rejectOld=reject;});
  const old=run('loadK()');
  context.fetch=normalFetch;run("cur.itv='5m';kLoaded=false;");await run('loadK()');
  rejectOld(Error('stale request failed'));await old;
  assert.equal(elements['chartpane-status'].hidden,true,'stale error covered newer chart');

  run("panelStatus('positions','loading','LOADING');panelStatus('lbbar','loading','LOADING');wsConnect();ws.onopen();");
  assert.equal(elements['positions-status'].hidden,false,'WS open hid unloaded positions');
  assert.equal(elements['lbbar-status'].hidden,false,'WS open hid unloaded leaderboard');
  context.statePacket=JSON.stringify({ok:true,type:'state',symbol:'FXSIM',price:1.1,bid:1.0999,ask:1.1001,quote_valid:true,balance:10000,equity:10000,used:0,leverage:100,margin_level:null,positions:[]});
  run('ws.onmessage({data:statePacket});');
  assert.equal(elements['positions-status'].hidden,true,'first WS state did not finish positions loading');
  assert.match(elements.pos.innerHTML,/暂无持仓，去下单吧/,'empty positions mistaken for not loaded');
  assert.equal(elements['lbbar-status'].hidden,false,'positions state hid unrelated leaderboard loader');
  run("ws.onmessage({data:JSON.stringify({type:'lb',rows:[]})});");
  assert.equal(elements['lbbar-status'].hidden,true);
  assert.match(elements.rankT.innerHTML,/暂无排名/);

  // Slow/failed HTTP after WS data cannot put successfully loaded panels back in error.
  run("panelStatus('lbbar','loading','LOADING');");
  let rejectRank;
  context.fetch=()=>new Promise((resolve,reject)=>{rejectRank=reject;});
  const rank=run('loadLeaderboard()');
  run('renderLb([])');rejectRank(Error('old HTTP failure'));await rank;
  assert.equal(elements['lbbar-status'].hidden,true,'late HTTP error hid WS ranking');
  run("panelStatus('positions','loading','LOADING');panelStatus('lbbar','loading','LOADING');");
  context.fetch=async()=>{throw Error('offline')};
  await run('refresh()');await run('loadLeaderboard()');
  assert.equal(elements['positions-status'].dataset.state,'error');
  assert.equal(elements['lbbar-status'].dataset.state,'error');
  context.fetch=async()=>({json:async()=>({ok:true,rows:[]})});await run('loadLeaderboard()');
  assert.equal(elements['lbbar-status'].hidden,true,'HTTP leaderboard success stayed loading');
  context.fetch=async()=>({json:async()=>JSON.parse(context.statePacket)});await run('refresh()');
  assert.equal(elements['positions-status'].hidden,true,'HTTP state success stayed loading');
  context.fetch=normalFetch;

  // Leverage edit is a draft: live state must not reset input, even while save is pending.
  context.levState={...JSON.parse(context.statePacket),leverage:5,positions:[]};
  run('levDirty=false;render(levState)');
  assert.equal(elements.lev.value,5);
  run("$('lev').value='10';levDirty=true;render(levState)");
  assert.equal(elements.lev.value,'10','live state overwrote unsaved leverage');
  let finishLev;
  context.fetch=(url,options)=>{
    assert.equal(url,'/api/leverage');assert.equal(JSON.parse(options.body).leverage,10);
    return new Promise(resolve=>{finishLev=resolve;});
  };
  const saving=run('setLev()');
  assert.equal(elements.lev.disabled,true);assert.equal(elements['lev-apply'].disabled,true);
  run('render(levState)');assert.equal(elements.lev.value,'10','state overwrote in-flight save');
  context.levState.leverage=10;
  context.fetch=async()=>({json:async()=>context.levState});
  finishLev({status:200,json:async()=>({ok:true})});await saving;
  assert.equal(run('accountLeverage'),10);assert.equal(run('levDirty'),false);
  assert.equal(elements.lev.disabled,false);assert.match(elements['lev-hint'].textContent,/10/);
  context.fetch=async()=>({status:400,json:async()=>({ok:false,err:'先平仓'})});
  run("$('lev').value='9';levDirty=true");await run('setLev()');
  assert.equal(elements.lev.value,10,'rejected leverage still appeared applied');
  const blocked=[];context.fetch=async url=>{blocked.push(url);throw Error('must not request');};
  run("$('lev').value='1.5';levDirty=true");await run('setLev()');
  assert.equal(blocked.length,0);
  run("$('lev').value='11';levDirty=true");await run('setLev()');
  assert.equal(blocked.length,0,'leverage above 10 reached the server');
  context.levState.positions=[{id:1,side:'long',lots:.1,sym:'FXSIM',entry:1.1,now:1.1,pnl:0,margin:11}];
  run('render(levState)');assert.equal(elements.lev.disabled,true);assert.equal(elements['lev-apply'].disabled,true);
  await run('setLev()');assert.equal(blocked.length,0,'in-position leverage changed');
  context.levState.positions=[];run('render(levState)');context.fetch=normalFetch;

  // NSTAR order UX: validate lots, block duplicates, keep pair read-only and unlock on errors.
  context.levState={...context.levState,symbol:'NSTAR',price:100,bid:99.97,ask:100.03,contract:.01};
  run('applySymbols({version:100,rows:[{symbol:"NSTAR",enabled:true}]});cur.sym="NSTAR";render(levState)');run('setLots(.01);syncOrderControls()');
  assert.equal(elements.lots.value,'0.01');assert.equal(elements['close-all'].disabled,true);
  let finishOrder,orderCalls=0;
  context.fetch=()=>{orderCalls++;return new Promise(resolve=>{finishOrder=resolve;});};
  const ordering=run('trade("buy")');
  assert.equal(elements['trade-buy'].disabled,true);assert.equal(elements.sym.disabled,true);
  await run('trade("buy")');assert.equal(orderCalls,1,'double click submitted duplicate order');
  run('render(levState)');assert.equal(elements['trade-buy'].disabled,true,'live state unlocked pending order');
  finishOrder({status:400,json:async()=>({ok:false,err:'余额不足'})});await ordering;
  assert.equal(run('orderBusy'),false);assert.equal(elements['trade-buy'].disabled,false);
  assert.match(elements.msg.textContent,/余额不足/);
  context.fetch=async()=>{throw Error('offline')};await run('trade("sell")');
  assert.equal(run('orderBusy'),false);assert.equal(elements['trade-sell'].disabled,false);
  elements.lots.reportValidity=()=>false;context.fetch=()=>{throw Error('invalid lots requested');};
  await run('trade("buy")');elements.lots.reportValidity=()=>true;
  run('curPos=[{id:99}];syncOrderControls()');assert.equal(elements['close-all'].disabled,false);
  context.confirm=()=>false;await run('closeAll()');assert.equal(run('orderBusy'),false);
  context.fetch=normalFetch;run('curPos=[];syncOrderControls()');

  // Chat history merges WS with late HTTP, escapes HTML, bounds rows and sends once.
  const chat={id:1,name:'<img>',time:1800000000000,text:'<script>alert(1)</script>'};
  context.chatPacket=JSON.stringify({type:'chat',row:chat});run('ws.onmessage({data:chatPacket})');
  assert.match(elements['chat-messages'].innerHTML,/&lt;script&gt;/);
  assert.doesNotMatch(elements['chat-messages'].innerHTML,/<script>/);
  context.fetch=async()=>({json:async()=>({ok:true,rows:[]})});await run('loadChat()');
  assert.equal(run('chatRows.length'),1,'late HTTP wiped WS chat');
  let finishChat;
  context.fetch=(url,options)=>new Promise(resolve=>{finishChat=resolve;});
  run("$('chat-text').value='hello'");const send=run('sendChat()');
  assert.equal(elements['chat-send'].disabled,true);
  const duplicate=run('sendChat()');await duplicate;
  finishChat({json:async()=>({ok:true,row:{...chat,id:2,text:'hello'}})});await send;
  assert.equal(elements['chat-text'].value,'');assert.equal(elements['chat-send'].disabled,false);
  run('ws.onmessage({data:JSON.stringify({type:"chat",row:{...JSON.parse(chatPacket).row,id:2,text:"hello"}})})');
  assert.equal(run('chatRows.length'),2,'HTTP/WS same message duplicated');
  context.fetch=async()=>({json:async()=>({ok:false,err:'发送太快'})});
  run("$('chat-text').value='keep draft'");await run('sendChat()');
  assert.equal(elements['chat-text'].value,'keep draft');assert.match(elements['chat-error'].textContent,/发送太快/);
  context.many=JSON.stringify(Array.from({length:110},(_,i)=>({...chat,id:i+1})));
  run('renderChat(JSON.parse(many))');assert.equal(run('chatRows.length'),100);
  context.fetch=normalFetch;

  // Realized history distinguishes forced close, escapes symbols and handles empty/error/retry.
  const ledger=[{id:10,time:1800000000000,kind:'stopout',position_id:9,symbol:'<img onerror="bad">',side:'long',lots:.1,entry:1,price:.9998,pnl:-2,balance:48,margin_level:.49},
    {id:9,time:1800000000000,kind:'close',position_id:8,symbol:'USDJPY',currency:'LEGACY',side:'long',lots:.1,entry:149,price:150,pnl:100,balance:50},
    {id:8,time:1800000000000,kind:'close',position_id:7,symbol:'BTCUSDT',side:'short',lots:.01,entry:60000,price:59900,pnl:1,balance:49}];
  context.fetch=async()=>({json:async()=>({ok:true,rows:ledger})});
  await run('loadHistory()');
  assert.equal(elements['history-status'].hidden,true);
  assert.match(elements.historyT.innerHTML,/强平/);assert.match(elements.historyT.innerHTML,/-\$2\.00/);
  assert.match(elements.historyT.innerHTML,/&lt;img/);assert.doesNotMatch(elements.historyT.innerHTML,/<img/);
  assert.match(elements.historyT.innerHTML,/USDJPY/);assert.match(elements.historyT.innerHTML,/旧口径 100\.00/);
  assert.match(elements.historyT.innerHTML,/BTC\/USDT/,'NSTAR-only UI filtered historical symbols');
  context.fetch=async()=>({json:async()=>({ok:true,rows:[]})});await run('loadHistory()');
  assert.match(elements.historyT.innerHTML,/暂无成交记录/);
  context.fetch=async()=>{throw Error('offline')};await run('loadHistory()');
  assert.equal(elements['history-status'].dataset.state,'error');
  context.fetch=async()=>({json:async()=>({ok:true,rows:ledger})});await run('loadHistory()');
  assert.equal(elements['history-status'].hidden,true);
  run("$('history').open=false;");
  context.latestTrade=JSON.stringify({...ledger[0],symbol:'FXSIM'});
  run('observeTrade(JSON.parse(latestTrade))');
  assert.match(elements.msg.textContent,/已触发强平 #9 FXSIM/);
  run("$('msg').textContent='ack';observeTrade(JSON.parse(latestTrade));");
  assert.equal(elements.msg.textContent,'ack','same WS state repeated stopout notification');
  run('render({...JSON.parse(statePacket),margin_level:0,stopout_level:.5})');
  assert.match(elements.acct.innerHTML,/保证金比例.*0%/);assert.match(elements.acct.innerHTML,/强平线.*50%/);
  assert.match(elements.acct.innerHTML,/\$10,000\.00/);
  assert.equal(run('money(1234567.89)'), '$1,234,567.89');
  run('observePrices("TESTJPY",[158.121,158.2])');
  assert.equal(run('priceText(158.2,"TESTJPY")'),'158.200');
  assert.equal(run('priceFormat("TESTJPY").minMove'),.001);
  assert.equal(run('decimalPlaces(1e-7)'),7);
  assert.equal(run('money(100)'), '$100.00');assert.equal(run('money(-2)'), '-$2.00');
  run("renderLb([{name:'<script>',equity:100,ret:.02,online:false,pnl_5m:-2,ret_5m:-.02}])");
  assert.match(elements.rankT.innerHTML,/class="offline" title="离线"/);
  assert.match(elements.rankT.innerHTML,/-\$2\.00/);assert.match(elements.rankT.innerHTML,/-2\.00%/);
  assert.doesNotMatch(elements.rankT.innerHTML,/<script>/);
  run("renderLb([{name:'test',equity:100,ret:.02,online:true,pnl_5m:null,ret_5m:null}])");
  assert.match(elements.rankT.innerHTML,/class="online" title="在线"/);assert.match(elements.rankT.innerHTML,/—/);
  context.fetch=normalFetch;

  // Splitter tracks pointer offset, respects bounds, and handles cancel/capture loss.
  const drag = code => run(code);
  const ratio = () => run('panelRatio');
  const key = key => {
    context.key = key;
    drag('splitter.onkeydown({key,preventDefault(){}})');
  };
  drag('splitter.onpointerdown({button:2,pointerId:1,clientY:500,preventDefault(){}})');
  assert.equal(run('splitDrag'), null, 'right mouse button started resizing');
  drag('splitter.onpointerdown({button:0,pointerId:1,clientY:500,preventDefault(){}})');
  assert.equal(elements.splitter.capture, 1);
  drag('splitter.onpointermove({pointerId:2,clientY:550})');
  assert.equal(ratio(), .68, 'foreign pointer resized panels');
  drag('splitter.onpointermove({pointerId:1,clientY:450})');
  assert.equal(ratio(), 350/590, 'drag jumped instead of preserving grab offset');
  drag('splitter.onpointermove({pointerId:1,clientY:5000})');
  assert.equal(ratio(), 420/590, 'positions/history panels collapsed');
  drag('splitter.onpointermove({pointerId:1,clientY:-5000})');
  assert.equal(ratio(), 120/590, 'chart panel collapsed');
  drag('splitter.onpointercancel({pointerId:1})');
  assert.equal(run('splitDrag'), null);
  assert.equal(elements.splitter.attributes['data-dragging'], undefined);
  assert.equal(context.localStorage.fx_panel_ratio, ratio());
  drag('splitter.ondblclick()');assert.equal(ratio(), .68);
  key('ArrowUp');assert.equal(ratio(), .65);
  key('ArrowDown');assert.equal(ratio(), .68);
  key('Home');assert.equal(ratio(), .15);
  key('End');assert.equal(ratio(), .9);
  assert.equal(elements.splitter.attributes['aria-valuenow'], '90');
  key('Escape');assert.equal(ratio(), .9);
  drag('splitter.ondblclick()');

  // Sidebars and history follow pointer offset, clamp, persist and handle keyboard/capture loss.
  run("$('tradebar');$('positions')");
  elements.tradebar.clientWidth=240;elements.rightbar.clientWidth=264;
  drag("$('left-splitter').onpointerdown({button:0,pointerId:4,clientX:240,preventDefault(){}})");
  drag("$('left-splitter').onpointermove({pointerId:4,clientX:280})");
  assert.equal(run('sideWidths.left'),280);
  assert.equal(elements.wrap.style['--trade-width'],'280px');
  drag("$('left-splitter').onpointermove({pointerId:5,clientX:300})");
  assert.equal(run('sideWidths.left'),280,'foreign pointer changed sidebar');
  drag("$('left-splitter').onpointermove({pointerId:4,clientX:2000})");
  assert.equal(run('sideWidths.left'),396,'center width below 320');
  drag("$('left-splitter').onpointercancel({pointerId:4})");
  assert.equal(JSON.parse(context.localStorage.fx_sidebar_widths).left,396);
  drag("$('left-splitter').ondblclick()");assert.equal(run('sideWidths.left'),240);
  drag("$('right-splitter').onpointerdown({button:0,pointerId:6,clientX:736,preventDefault(){}})");
  drag("$('right-splitter').onpointermove({pointerId:6,clientX:700})");
  assert.equal(run('sideWidths.right'),300,'right separator direction reversed');
  drag("$('right-splitter').onlostpointercapture({pointerId:6})");
  assert.equal(JSON.parse(context.localStorage.fx_sidebar_widths).right,300);
  elements.rightbar.clientWidth=300;
  drag("$('right-splitter').onkeydown({key:'ArrowLeft',preventDefault(){}})");
  assert.equal(run('sideWidths.right'),316);
  drag("$('right-splitter').ondblclick()");assert.equal(run('sideWidths.right'),264);
  elements.positions.clientHeight=234;
  drag("$('history-splitter').onpointerdown({button:0,pointerId:7,clientY:500,preventDefault(){}})");
  drag("$('history-splitter').onpointermove({pointerId:7,clientY:530})");
  assert.equal(run('historyRatio'),264/390,'history divider jumped instead of preserving grab offset');
  drag("$('history-splitter').onpointermove({pointerId:7,clientY:-1000})");
  assert.equal(run('historyRatio'),80/390,'positions collapsed');
  drag("$('history-splitter').onpointermove({pointerId:7,clientY:5000})");
  assert.equal(run('historyRatio'),310/390,'history collapsed');
  drag("$('history-splitter').onpointerup({pointerId:7})");
  assert.equal(context.localStorage.fx_history_ratio,310/390);
  drag("$('history-splitter').ondblclick()");assert.equal(run('historyRatio'),.6);

  elements.lbbar.clientHeight=195;
  drag("$('chat-splitter').onpointerdown({button:0,pointerId:8,clientY:195,preventDefault(){}})");
  drag("$('chat-splitter').onpointermove({pointerId:8,clientY:215})");
  assert.equal(run('chatRatio'),215/390);
  drag("$('chat-splitter').onpointermove({pointerId:8,clientY:1000})");
  assert.equal(run('chatRatio'),230/390,'chat form collapsed');
  drag("$('chat-splitter').onpointerup({pointerId:8})");
  assert.equal(context.localStorage.fx_chat_ratio,230/390);
  drag("$('chat-splitter').ondblclick()");assert.equal(run('chatRatio'),.5);

  // Saved layout applies in a fresh page; invalid values do not poison CSS.
  const saved=context.localStorage;
  const otherElements={};
  const fresh=vm.createContext({...context,localStorage:{...saved,fx_sidebar_widths:'{"left":300,"right":260}',fx_history_ratio:.7},
    document:{...documentMock,querySelectorAll() { return []; },getElementById(id){return otherElements[id]||=( {...elements.splitter,style:{setProperty(key,value){this[key]=value;}},attributes:{},clientWidth:1000,clientHeight:600});}}});
  vm.runInContext(script,fresh);
  assert.equal(vm.runInContext('sideWidths.left',fresh),300);
  assert.equal(vm.runInContext('sideWidths.right',fresh),260);
  assert.equal(vm.runInContext('historyRatio',fresh),.7);
  assert.equal(vm.runInContext('panelRatio',fresh),.68);

  // Container resizing must adjust chart HEIGHT as well as width (not fixed 480).
  let observe, applied;
  Object.assign(context, {
    LightweightCharts:{CandlestickSeries:{type:'Candlestick'},version:()=> '5.2.1',createChart(el,options){
      assert.equal(options.height,400);
      assert.equal(options.layout.attributionLogo,false,'TradingView logo is back on the chart');
      return {subscribeCrosshairMove(){},addSeries(definition,options){assert.equal(definition.type,'Candlestick');assert.equal(options.wickUpColor,'#25a750');return series;},applyOptions(options){applied=options;}};
    }},
    ResizeObserver:class {constructor(callback){this.callback=callback;}observe(el){observe=this.callback;assert.equal(el,elements.cv);}},
  });
  run('chart=null;initChart()');
  elements.cv.clientHeight=250;elements.cv.clientWidth=700;observe();flushFrames();
  assert.equal(applied.height,250);assert.equal(applied.width,700);
  elements.cv.clientHeight=0;observe();flushFrames();assert.equal(applied.height,250, 'hidden chart was resized to zero');
  run('chart=testChart');

  // Legacy position lines remain filtered by the current chart without a UI switch entry.
  removed=[];
  run("cur.sym='NSTAR';priceLines=new Map([[99,{line:{price:60000}}]]);curPos=[{sym:'BTCUSDT',entry:60000,side:'long',lots:.1}];drawPosLines()");
  assert.equal(removed.length, 1, 'legacy position line survives on NSTAR chart');
  assert.equal(run('priceLines.size'), 0);
  // Quote styling uses native precision; tick direction and daily change are independent.
  context.fetch=async url=>{assert.match(url,/interval=1d&limit=2$/);return {json:async()=>({ok:true,klines:[[0,158,159,157,158.091],[86400000,158.091,159,157,158.290]]})};};
  run("cur.sym='USDJPY';pricePrecisions.set(cur.sym,3);quoteReferences.clear();renderQuote(158.290,cur.sym)");
  assert.equal(elements['quote-change'].textContent,'','missing baseline must not fabricate change');
  await tick();
  assert.equal(elements.price.innerHTML,'158<span class="quote-decimal">.290</span>');
  assert.equal(elements['quote-unit'].textContent,'JPY');
  assert.equal(context.document.title,'158.290 | USDJPY');
  assert.equal(elements['quote-change'].textContent,'+0.199  +0.13%');
  run("renderQuote(158.300,'USDJPY')");assert.equal(elements.price.dataset.direction,'up');
  run("renderQuote(158.290,'USDJPY')");assert.equal(elements.price.dataset.direction,'down');
  assert.match(elements['quote-change'].className,/green/,'falling tick must not change positive daily gain to red');
  run("renderQuote(158.290,'USDJPY')");assert.equal(elements.price.dataset.direction,'down','duplicate state erased direction');
  run("renderQuote(158.000,'USDJPY')");assert.match(elements['quote-change'].className,/red/);
  assert.equal(elements['quote-change'].textContent,'-0.091  -0.06%');

  let dailyReply;
  context.fetch=()=>new Promise(resolve=>{dailyReply=resolve;});
  run("quoteReferences.clear();renderQuote(158.290,'USDJPY');cur.sym='BTCUSDT';renderQuote(NaN,cur.sym)");
  dailyReply({json:async()=>({ok:true,klines:[[0,158,159,157,158.091],[86400000,158.091,159,157,158.290]]})});
  await tick();
  assert.equal(elements['quote-change'].textContent,'','stale daily response leaked across symbols');
  context.fetch=async()=>({json:async()=>({ok:false})});
  run("wsConnect();ws.onmessage({data:JSON.stringify({sym:'BTCUSDT',price:60000.75})})");
  await tick();
  assert.equal(elements.price.innerHTML,'60,000<span class="quote-decimal">.75</span>');
  assert.equal(elements['quote-unit'].textContent,'USDT');
  assert.equal(elements['quote-change'].textContent,'','unavailable daily baseline must stay blank');
  run("ws.onmessage({data:JSON.stringify({sym:'USDJPY',price:158.290})})");
  assert.equal(elements['quote-unit'].textContent,'USDT','stale tick changed active quote');
  assert.equal(context.document.title,'60000.75 | BTCUSDT','title must follow WS quote and ignore stale symbols');

  // Weekly FX history and live bars share UTC Monday, not Unix epoch Thursday.
  const savedNow=now;now=Date.parse('2026-10-07T12:00:00Z');
  run("cur={sym:'USDCNH',itv:'1w'};lastSym=cur.sym;lastItv=cur.itv;kLoaded=true;lastBar={time:Date.parse('2026-10-05T00:00:00Z')/1000,open:6.7,high:6.71,low:6.69,close:6.7}");
  displayed=[{...run('lastBar')}];run('updateLast(6.706195)');
  assert.equal(updates.at(-1).time,Date.parse('2026-10-05T00:00:00Z')/1000);
  assert.equal(displayed.length,1,'live tick created Thursday candle beside Monday week');now=savedNow;

  const fs=require('node:fs');
  const html=fs.readFileSync('static/index.html','utf8');
  assert.match(html,/<title>— \| NSTAR\/USDT<\/title>/);
  assert.match(html,/<select id="sym" disabled><option value="NSTAR">NSTAR\/USDT · 北极星币<\/option><\/select>/);
  assert.match(html,/<b id="order-symbol">NSTAR\/USDT 永续<\/b>/);
  assert.doesNotMatch(html,/USDJPY|BTCUSDT|FXSIM|真实行情/,'production HTML advertised legacy markets');
  assert.match(html,/id="mode-perp"/);assert.match(html,/id="mode-spot"/);
  assert.match(html,/id="stat-funding"/);assert.match(html,/id="tier-hint"/);
  assert.match(html,/id="chartpane"[^]*?id="msg"[^]*?id="chartpane-status"/);
  assert.match(fs.readFileSync('static/app.css','utf8'),/#msg\{position:absolute;/,'toast must not occupy chart layout space');
  assert.match(fs.readFileSync('static/index.html','utf8'),/id="trade-buy"[^>]*>BUY<small id="buy-price">—<\/small><\/button>/);
  assert.match(fs.readFileSync('static/index.html','utf8'),/id="trade-sell"[^>]*>SELL<small id="sell-price">—<\/small><\/button>/);
  // Button prices use source sides, never mid; stale/old-symbol data cannot enable trading.
  context.fetch=async()=>({json:async()=>({ok:false})});
  run("cur.sym='USDCNH';applySymbols({version:200,rows:[{symbol:'USDCNH',enabled:true}]});renderQuote(6.706195,cur.sym,6.70493,6.70746,true)");
  assert.equal(elements['buy-price'].textContent,'6.707460');
  assert.equal(elements['sell-price'].textContent,'6.704930');
  assert.equal(elements['trade-buy'].disabled,true,'legacy quote must not enable new trading');
  assert.equal(run('quoteReady'),true,'compatibility quote validation was lost');
  run("renderQuote(158,'USDJPY',157.99,158.01,true)");
  assert.equal(elements['buy-price'].textContent,'6.707460','old symbol changed active ask');
  run("renderQuote(6.706195,'USDCNH',6.70493,6.70746,false)");
  assert.equal(elements['buy-price'].textContent,'—');assert.equal(elements['trade-buy'].disabled,true);
  run("renderQuote(NaN,'USDCNH')");assert.equal(elements['sell-price'].textContent,'—');
  const savedTimers={set:context.setTimeout,clear:context.clearTimeout};let expire;
  context.setTimeout=(callback,ms)=>{assert.equal(ms,30000);expire=callback;return 1;};
  run("renderQuote(6.706195,'USDCNH',6.70493,6.70746,true,Date.now())");
  expire();flushFrames();assert.equal(elements['buy-price'].textContent,'—');assert.equal(elements['trade-sell'].disabled,true);
  context.setTimeout=savedTimers.set;context.clearTimeout=savedTimers.clear;
  run("cur.sym='BTCUSDT';pricePrecisions.set(cur.sym,2);renderQuote(60000.25,cur.sym,60000.249900000004,60000.2501,true)");
  assert.equal(elements['buy-price'].textContent,'60000.2501');assert.equal(elements['sell-price'].textContent,'60000.2499');
  assert.equal(run("pricePrecisions.get('BTCUSDT')"),2,'synthetic spread polluted native chart precision');
  // Exchange UI: NSTAR/USDT naming, pinned precision, OKX-style positions, spot assets, book and tape.
  assert.equal(run("pairName('NSTAR')"),'NSTAR/USDT');assert.equal(run("pairName('BTCUSDT')"),'BTC/USDT');
  run("cur.sym='NSTAR';kLoaded=false;observePrices('NSTAR',[101.24453000000001])");
  assert.equal(run("priceText(101.24453000000001,'NSTAR')"),'101.24','NSTAR fills leaked float noise');
  context.exState=JSON.stringify({ok:true,type:'state',symbol:'NSTAR',price:100,bid:99.97,ask:100.03,quote_valid:true,balance:9000,equity:9100,used:20,
    leverage:10,margin_level:4.5,contract:.01,symbol_leverage:10,nstar_price:100,spot_fee:.001,
    spot:[{sym:'NSTAR',units:2,cost:95,price:100,value:199.6,pnl:9.6}],
    positions:[{id:7,side:'long',lots:2000,sym:'NSTAR',entry:99,now:100.1,pnl:22,margin:20,units:20,leverage:10,roe:1.1,tp:120,sl:0,liq:95.5}]});
  run('render(JSON.parse(exState))');
  assert.match(elements.pos.innerHTML,/NSTAR\/USDT/);assert.match(elements.pos.innerHTML,/95\.50/);
  assert.match(elements.pos.innerHTML,/110\.00%/,'ROE missing');assert.match(elements.pos.innerHTML,/openTpsl\(7\)/);
  assert.match(elements.spotT.innerHTML,/NSTAR/);assert.equal(elements['spot-count'].textContent,1);
  assert.equal(elements['quote-unit'].textContent,'USDT');
  run("$('lots').value='1000';renderOrderEstimate()");
  assert.equal(elements['order-notional'].textContent,'1,000 USDT');assert.match(elements['order-margin'].textContent,/^100 USDT · 10x$/);
  run("renderBook(100)");
  assert.match(elements['book-asks'].innerHTML,/100\.05/);assert.match(elements['book-bids'].innerHTML,/99\.95/);
  run("renderTape([[1800000000000,100.5,3],[1800000000000,99.5,-2]])");
  assert.match(elements['tape-rows'].innerHTML,/t-buy">100\.50/);assert.match(elements['tape-rows'].innerHTML,/t-sell">99\.50/);
  run("renderTape([])");assert.match(elements['tape-rows'].innerHTML,/暂无成交/);
  // Mutations render the state embedded in their reply instead of issuing GET /api/state.
  const before=requests.length;
  context.fetch=async url=>{requests.push(url);return {status:200,json:async()=>({ok:true,state:{...JSON.parse(context.exState),balance:4321}})};};
  run("orderBusy=false;applySymbols({version:300,rows:[{symbol:'NSTAR',enabled:true}]});cur.sym='NSTAR';renderQuote(100,'NSTAR',99.97,100.03,true)");
  await run("trade('buy')");
  assert.deepEqual(requests.slice(before),['/api/trade'],'order reply still triggered a state poll');
  assert.match(elements.acct.innerHTML,/\$4,321\.00/);
  context.fetch=async url=>{requests.push(url);return {status:200,json:async()=>({ok:true})};};
  const noState=requests.length;await run("trade('buy')");
  assert.deepEqual(requests.slice(noState),['/api/trade','/api/state'],'missing state must fall back to GET');
  // Spot condition orders use /api/order with units and an explicit market, not perpetual lots.
  let spotBody;
  context.fetch=async(url,options)=>{requests.push(url);spotBody=JSON.parse(options.body);return {status:200,json:async()=>({ok:true,state:JSON.parse(context.exState)})};};
  run("orderBusy=false;cur.sym='NSTAR';quoteReady=true;setSpotSide('sell');setSpotType('stop');$('spot-units').value='1.25';$('spot-trigger').value='95';$('spot-price').value='94'");
  assert.equal(elements['spot-trigger-field'].hidden,false);assert.equal(elements['spot-price-field'].hidden,false);
  await run('spotTrade()');
  assert.equal(requests.at(-1),'/api/order');
  assert.deepEqual({...spotBody},{market:'spot',kind:'stop',side:'sell',units:1.25,price:95,limit:94});
  run("renderOrders({orders:[{id:2,kind:'limit',side:'sell',spot:true,symbol:'NSTAR',lots:125,price:94,limit:0,tp:0,sl:0,created:1800000000000}]})");
  assert.match(elements.ordersT.innerHTML,/限价 · 现货/);assert.doesNotMatch(elements.ordersT.innerHTML,/卖出开空/);
  run("setSpotType('market')");assert.equal(elements['spot-trigger-field'].hidden,true);assert.equal(elements['spot-price-field'].hidden,true);
  // A brand-new account's first trade still reloads history (cursor aligned on empty state).
  run('tradeCursor=false;seenTrade=0;historyLoaded=true;observeTrade(undefined)');
  const hist=requests.length;run("observeTrade({id:1,kind:'open'})");
  assert.equal(requests.slice(hist).filter(u=>u==='/api/history').length,1,'first trade of a new account skipped history');
  // Polling pauses while the socket is live or the tab is hidden.
  run('wsLive=true');assert.equal(run('pollNeeded()'),false);
  run('wsLive=false');assert.equal(run('pollNeeded()'),true);
  context.fetch=normalFetch;

  // Incremental simulated candles: 50 current-bar ticks are updates, not 500-bar resets.
  context.fetch=normalFetch;now=t*1000;
  run("cur={sym:'NSTAR',itv:'1m'};kLoaded=false;pendingSim.clear();pendingSimStamp=0;simStamp=0");
  response=[[t*1000,100,101,99,100]];responseStamp=now;await run('loadK()');
  const initialSets=sets;
  for(let i=1;i<=50;i++)run(`updateSim([[${t*1000},100,${101+i/100},99,${100+i/100}]],${now+i})`);
  assert.equal(sets,initialSets,'50 live updates invoked setData');
  assert.equal(displayed.at(-1).close,100.5);
  run(`updateSim([[${(t+60)*1000},101,102,100,101]],${now+60000})`);
  assert.equal(sets,initialSets,'new consecutive bar invoked setData');
  const staleSnapshot=JSON.stringify(displayed);
  run(`updateSim([[${t*1000},100,999,1,999]],${now-1})`);
  assert.equal(JSON.stringify(displayed),staleSnapshot,'old packet bypassed timestamp guard');
  run(`updateSim([[${t*1000},100,105,95,100.5],[${(t+60)*1000},101,102,100,101]],${now+60001})`);
  assert.equal(sets,initialSets+1,'closed-bar correction did not refresh history');
  assert.equal(displayed[0].high,105);assert.equal(displayed[0].low,95);
  const beforeTrim=sets;
  for(let i=2;i<=700;i++)run(`updateSim([[${(t+i*60)*1000},101,102,100,101]],${now+i*60000})`);
  assert.ok(run('simBars.length')<=564,'simulated history grew without bound');
  assert.ok(sets-beforeTrim<=4,'bar retention reset data every new candle');

  // Full-history fetches are singleflight; changing interval aborts the old fetch.
  let resolveK,oldSignal;const kCalls=[];
  context.fetch=(url,options)=>{kCalls.push(url);oldSignal=options.signal;return new Promise(resolve=>{resolveK=resolve;});};
  const oneK=raw('loadK()'),sameK=raw('loadK()');assert.equal(oneK,sameK);assert.equal(kCalls.length,1);
  const finishK=resolveK,signalK=oldSignal;
  raw("cur.itv='5m';kLoaded=false");const newK=raw('loadK()');assert.equal(signalK.aborted,true,'superseded K fetch was not aborted');
  resolveK({json:async()=>({ok:true,klines:response,updated_at:now+1})});await newK;
  finishK({json:async()=>({ok:true,klines:response,updated_at:now})});await oneK;flushFrames();
  assert.equal(run('lastItv'),'5m');context.fetch=normalFetch;

  // Same state does not rebuild tables/account or position lines. Price lines use stable IDs.
  run('cur.sym="NSTAR";render(JSON.parse(exState))');
  const tableWrites=[elements.pos,elements.ordersT,elements.spotT,elements.acct].map(n=>n.htmlWrites);
  const line=raw('priceLines.get(7).line'),removedBefore=removed.length;
  run('render(JSON.parse(exState))');
  assert.deepEqual([elements.pos,elements.ordersT,elements.spotT,elements.acct].map(n=>n.htmlWrites),tableWrites);
  assert.equal(raw('priceLines.get(7).line'),line);assert.equal(removed.length,removedBefore);
  run('render({...JSON.parse(exState),positions:[{...JSON.parse(exState).positions[0],entry:98}]})');
  assert.equal(raw('priceLines.get(7).line'),line);assert.equal(line.changes,1);
  run('render({...JSON.parse(exState),positions:[]})');assert.equal(removed.length,removedBefore+1);
  run('renderBook(100)');const asks=[...elements['book-asks'].children],bids=[...elements['book-bids'].children];
  run('renderBook(101)');assert.equal(asks.length,11);assert.equal(bids.length,11);
  assert.deepEqual(elements['book-asks'].children,asks);assert.deepEqual(elements['book-bids'].children,bids);
  run('renderTape([[1800000000000,100,1]],42)');const tapeWrites=elements['tape-rows'].htmlWrites;
  run('renderTape([[1800000000000,101,2]],42)');assert.equal(elements['tape-rows'].htmlWrites,tapeWrites,'same tape sequence repainted');
  run('renderTape([[1800000000001,101,2]],43)');assert.equal(elements['tape-rows'].htmlWrites,tapeWrites+1);
  assert.equal(elements['spot-price'].listeners.input.length,1,'estimate rebound spot input listener');

  // Chat only inserts new IDs, preserves existing nodes, crops at 100, and respects scroll position.
  run('chatRows=[];chatNodes.clear();renderChat([{id:201,name:"a",time:Date.now(),text:"one"}])');
  const firstChat=elements['chat-messages'].children[0],chatHtmlWrites=elements['chat-messages'].htmlWrites;
  elements['chat-messages'].scrollHeight=1000;elements['chat-messages'].clientHeight=200;elements['chat-messages'].scrollTop=50;
  run('renderChat([{id:202,name:"b",time:Date.now(),text:"two"}])');
  assert.equal(elements['chat-messages'].children[0],firstChat);assert.equal(elements['chat-messages'].scrollTop,50);
  assert.equal(elements['chat-messages'].htmlWrites,chatHtmlWrites,'chat append replaced the whole list');
  run('renderChat([{id:202,name:"b",time:Date.now(),text:"two"}])');assert.equal(elements['chat-messages'].children.length,2);
  elements['chat-messages'].scrollTop=800;
  run('renderChat([{id:203,name:"c",time:Date.now(),text:"three"}])');assert.equal(elements['chat-messages'].scrollTop,1000);
  run('renderChat(Array.from({length:110},(_,i)=>({id:204+i,name:"c",time:Date.now(),text:"x"})))');
  assert.equal(elements['chat-messages'].children.length,100);assert.equal(run('chatNodes.size'),100);

  // Hidden panels retain latest state but do not mutate their DOM until selected.
  const posPane={hidden:true},bookPane={hidden:true},chatPane={hidden:true},rankPane={hidden:true},tapePane={hidden:true};
  panels['[data-lower-pane="pos"]']=posPane;panels['[data-upper-pane="book"]']=bookPane;
  panels['[data-chat-pane="chat"]']=chatPane;panels['[data-upper-pane="rank"]']=rankPane;panels['[data-chat-pane="tape"]']=tapePane;
  const hiddenWrites=[elements.pos,elements.rankT,elements['tape-rows'],elements['chat-messages']].map(n=>n.htmlWrites);
  const hiddenBook=elements['book-asks'].children[0].children[0].textContent;
  run('render(JSON.parse(exState));renderLb([{name:"new",rank:1,equity:1,ret:0}]);renderBook(102);renderTape([[Date.now(),102,3]],44);renderChat([{id:999,name:"x",text:"hidden",time:Date.now()}])');
  assert.deepEqual([elements.pos,elements.rankT,elements['tape-rows'],elements['chat-messages']].map(n=>n.htmlWrites),hiddenWrites);
  assert.equal(elements['book-asks'].children[0].children[0].textContent,hiddenBook);
  for(const panel of Object.values(panels))panel.hidden=false;
  run('scheduleVisual("state",()=>paintState(lastState));requestRender()');
  assert.match(elements['chat-messages'].innerHTML,/hidden/);assert.match(elements.pos.innerHTML,/openTpsl/);

  // All hidden WS packets leave the DOM alone, but account safety/trade notifications still ingest.
  run('wsConnect();ws.readyState=1;ws.onopen();tradeCursor=true;seenTrade=999;historyLoaded=true');
  await tick();documentMock.hidden=true;raw('visibilityChanged()');const hiddenDOM=domWrites,hiddenSets=sets,hiddenRequests=requests.length;
  raw('ws.onmessage({data:JSON.stringify({type:"state",...JSON.parse(exState),last_trade:{id:1000,kind:"stopout",currency:"USD",position_id:7,symbol:"NSTAR",margin_level:.1,pnl:-5}})})');
  raw('ws.onmessage({data:JSON.stringify({type:"symbols",version:400,rows:[{symbol:"NSTAR",enabled:false}]})})');
  raw('ws.onmessage({data:JSON.stringify({type:"quote",sym:"NSTAR",price:103,bid:102,ask:104,quote_valid:true,quote_at:Date.now(),time:Date.now(),klines:{"5m":[[Date.now(),103,104,102,103]]},market:{tape:[],tape_seq:45}})})');
  raw('ws.onmessage({data:JSON.stringify({type:"chat",row:{id:1001,name:"x",text:"background",time:Date.now()}})})');
  raw('ws.onmessage({data:JSON.stringify({type:"lb",rows:[],total:200,as_of:Date.now()})})');
  raw('ws.onmessage({data:JSON.stringify({type:"nstar",kind:"rumor",text:"test",time:Date.now()})})');
  raw('ws.onmessage({data:JSON.stringify({type:"resync",history:true,chat:true})})');
  assert.equal(raw('chatDirty'),true);
  flushFrames();assert.equal(domWrites,hiddenDOM,'hidden WS mutated the DOM');assert.equal(sets,hiddenSets);
  assert.equal(requests.length,hiddenRequests,'hidden state triggered history/daily HTTP polling');
  assert.equal(raw('historyDirty'),true);assert.equal(raw('seenTrade'),1000);assert.equal(frames.size,0);
  assert.equal(raw('ws.sent.at(-1).visible'),false);
  const resumeRequests=[];
  context.fetch=async(url)=>{resumeRequests.push(url);return {json:async()=>url==='/api/state'?JSON.parse(context.exState):url.includes('/api/klines')?{ok:true,klines:response,updated_at:now}:{ok:true,rows:[],total:200}};};
  documentMock.hidden=false;raw('visibilityChanged()');assert.equal(frames.size,1,'resume scheduled duplicate frames');await tick();
  assert.match(elements.msg.textContent,/已触发强平 #7 NSTAR/,'background trade toast was lost');
  assert.equal(resumeRequests.filter(url=>url.includes('/api/klines')).length,1,'resume fetched unbounded K history');
  assert.equal(resumeRequests.filter(url=>url==='/api/history').length,1);assert.equal(raw('historyDirty'),false);
  assert.equal(resumeRequests.filter(url=>url==='/api/chat').length,1);assert.equal(raw('chatDirty'),false);
  assert.equal(raw('ws.sent.at(-1).visible'),true);

  // Low power is persisted, sends server subscription control and caps visible updates at 2Hz.
  const timers=new Map(),oldSet=context.setTimeout,oldClear=context.clearTimeout;let timerId=0;
  context.setTimeout=(callback,ms)=>{timers.set(++timerId,{callback,ms});return timerId;};context.clearTimeout=id=>timers.delete(id);
  run("$('low-power').checked=true;$('low-power').onchange()");
  assert.equal(context.localStorage.fx_low_power,'true');assert.equal(raw('ws.sent.at(-1).low_power'),true);
  assert.equal(raw('ws.sent.at(-1).interval'),'5m');
  raw('ws.onmessage({data:JSON.stringify({sym:"NSTAR",price:110,bid:109,ask:111,quote_valid:true,quote_at:Date.now()})})');
  assert.equal(frames.size,0,'low power bypassed 500ms gate');
  const delayed=[...timers.values()].find(x=>x.ms>0&&x.ms<=500);assert.ok(delayed);
  now+=500;delayed.callback();flushFrames();assert.match(elements.price.innerHTML,/110/);
  raw('ws.onmessage({data:JSON.stringify({sym:"NSTAR",price:111,bid:110,ask:112,quote_valid:true,quote_at:Date.now()})})');
  assert.equal(frames.size,0);run("$('low-power').checked=false;$('low-power').onchange()");assert.match(elements.price.innerHTML,/111/);
  assert.equal(context.localStorage.fx_low_power,'false');context.setTimeout=oldSet;context.clearTimeout=oldClear;

  // Shared Top50 cannot erase an independently delivered personal rank; pagination is auth + on demand.
  run('ME="self";render({...JSON.parse(exState),leaderboard_me:{name:"self",rank:123,equity:4321,ret:0}})');
  run('renderLb(Array.from({length:50},(_,i)=>({name:"u"+i,rank:i+1,equity:100,ret:0})),{total:201,as_of:Date.now()+1})');
  assert.match(elements['rank-me'].textContent,/#123/);assert.match(elements['rank-me'].textContent,/4,321/);
  const lbCalls=[];
  context.fetch=async(url,options)=>{lbCalls.push(url);assert.match(options.headers.Authorization,/^Bearer /);return {json:async()=>({ok:true,rows:[{name:'page2',rank:51,equity:1,ret:0}],me:{name:'self',rank:124,equity:4321},total:201,as_of:now+2})};};
  run('leaderboardPage(1)');await tick();assert.deepEqual(lbCalls,['/api/leaderboard?offset=50&limit=50']);
  assert.match(elements.rankT.innerHTML,/>51</);assert.match(elements['rank-me'].textContent,/#124/);
  rankPane.hidden=true;await run('loadLeaderboard()');assert.equal(lbCalls.length,1,'hidden ranking requested HTTP');rankPane.hidden=false;

  // NSTAR daily change comes from an actual closed-day reference, with no extra K request.
  const refRequests=[];context.fetch=async url=>{refRequests.push(url);return {json:async()=>({ok:false})};};
  run('cur.sym="NSTAR";renderQuote(110,"NSTAR",109,111,true,Date.now());renderMarket({prev_close:100,tape:[]})');
  assert.equal(elements['quote-change'].textContent,'+10.00  +10.00%');assert.equal(refRequests.length,0);
  run('renderMarket({prev_close:null,open_24h:90,tape:[]})');assert.equal(elements['quote-change'].textContent,'','open24h must not impersonate prev_close');
  // Quote timestamp guards survive throttled timers; expired spot/perp clicks never submit.
  run('applySymbols({version:401,rows:[{symbol:"NSTAR",enabled:true}]});renderQuote(110,"NSTAR",109,111,true,Date.now())');
  now+=30001;await run('trade("buy")');await run('spotTrade()');assert.equal(refRequests.length,0,'expired quote submitted an order');
  assert.equal(raw('quoteReady'),false);
  raw('acceptQuote(110,"NSTAR",109,111,true,Date.now());acceptQuote(1,"NSTAR",.9,1.1,true,Date.now()-1)');flushFrames();
  assert.match(elements.price.innerHTML,/110/,'old quote overrode latest after coalescing');
  // 401 still invalidates the account while the document is hidden.
  context.localStorage.fx_token='expired';context.localStorage.removeItem=key=>delete context.localStorage[key];
  context.fetch=async()=>({status:401,json:async()=>({ok:false})});documentMock.hidden=true;await raw('refresh()');
  assert.equal(context.location.reloaded,true);assert.equal(context.localStorage.fx_token,undefined);documentMock.hidden=false;flushFrames();

  // Silence timeout is 60s, never a normal 500ms tick; reconnect delay grows with jitter.
  raw('wsLive=true;wsLastMessage=Date.now();wsOpenedAt=Date.now();ws.closed=false');now+=500;run('checkSocket()');assert.equal(raw('ws.closed'),false);
  now+=59000;run('checkSocket()');assert.equal(raw('ws.closed'),false);
  now+=1000;run('checkSocket()');assert.equal(raw('ws.closed'),true);
  const delays=[];context.setTimeout=(callback,ms)=>{delays.push(ms);return delays.length;};
  raw('wsAttempts=0;wsTimer=null;reconnectLater();wsTimer=null;reconnectLater();wsTimer=null;reconnectLater()');
  assert.ok(delays[0]>=750&&delays[0]<=1250);assert.ok(delays[1]>=1500&&delays[1]<=2500);assert.ok(delays[2]>=3000&&delays[2]<=5000);
  context.setTimeout=oldSet;

  // First boot/open fetches each seed once, and NSTAR has exactly one K request (no dailyRef).
  const initRequests=[],initEls={},initFrames=new Map();let initId=0;
  const initDoc={...documentMock,hidden:false,getElementById(id){return initEls[id]||=mockNode('div',id);},querySelector(selector){return /upper-pane="rank"/.test(selector)?{hidden:true}:null;}};
  const initContext=vm.createContext({...context,localStorage:{fx_token:'test',fx_low_power:'true'},document:initDoc,
    requestAnimationFrame(fn){initFrames.set(++initId,fn);return initId;},cancelAnimationFrame(id){initFrames.delete(id);},
    ResizeObserver:class{observe(){}},LightweightCharts:undefined,
    fetch:async(url)=>{initRequests.push(url);return {json:async()=>url==='/api/me'?{ok:true,name:'tester'}:url==='/api/state'?JSON.parse(context.exState):url==='/api/symbols'?{ok:true,version:1,rows:[{symbol:'NSTAR',enabled:true}]}:url.includes('/api/klines')?{ok:true,klines:response,updated_at:now}:{ok:true,rows:[]}};}});
  // Disable autoboot only until the chart doubles are injected.
  initContext.localStorage.fx_token='';vm.runInContext(script,initContext);
  Object.assign(initContext,{testChart:chart,testSeries:series});vm.runInContext('TOKEN="test";chart=testChart;series=testSeries',initContext);
  await vm.runInContext('boot()',initContext);await tick();vm.runInContext('ws.readyState=1;ws.onopen()',initContext);await tick();
  for(const fn of initFrames.values())fn();initFrames.clear();await tick();
  for(const endpoint of ['/api/symbols','/api/state','/api/chat'])assert.equal(initRequests.filter(url=>url===endpoint).length,1,endpoint+' duplicated on open');
  assert.equal(initRequests.filter(url=>url.includes('/api/klines')).length,1,'NSTAR boot fetched duplicate/daily K');
  assert.equal(initRequests.some(url=>url.includes('/api/leaderboard')),false,'hidden initial ranking fetched');
  assert.equal(vm.runInContext('lowPower',initContext),true);assert.equal(vm.runInContext('ws.sent[0].low_power',initContext),true);
  assert.match(html,/label[^>]*for="low-power"/);
  // Library and history requests begin together, before either resolves.
  const parallel=makeLoader(),parallelRequests=[];let parallelReply;
  parallel.sandbox.fetch=url=>{parallelRequests.push(url);return new Promise(resolve=>{parallelReply=resolve;});};
  vm.runInContext('msg=()=>{}',parallel.sandbox);
  const parallelLoad=vm.runInContext('loadK()',parallel.sandbox);
  assert.equal(parallelRequests.length,1);assert.equal(parallel.scripts.length,1,'chart library waited for HTTP K');
  parallel.ready();parallel.scripts[0].onload();parallelReply({json:async()=>({ok:false,err:'test'})});await parallelLoad;
  context.fetch=normalFetch;

  // Admin accepts the new one-row API, filters mixed responses and retains old audit symbols.
  const adminElements={},adminRequests=[];
  const node=()=>({children:[],attributes:{},value:'',textContent:'',hidden:false,disabled:false,
    append(...children){this.children.push(...children);},add(option){this.children.push(option);},
    replaceChildren(){this.children=[];},setAttribute(key,value){this.attributes[key]=String(value);}});
  let adminRows=[{symbol:'NSTAR',provider:'simulation',enabled:true}];
  const admin=vm.createContext({document:{getElementById(id){return adminElements[id]||=node();},createElement:node},
    sessionStorage:{},Option:context.Option,AbortSignal,
    fetch:async(url,options)=>{
      adminRequests.push(url);
      if(url==='/api/admin/users')return {json:async()=>({ok:true,rows:[{name:'tester',balance:10000,deposited:0,positions:0}]})};
      if(url==='/api/admin/deposits')return {json:async()=>({ok:true,rows:[{time:now,kind:'symbol',enabled:false,name:'USDJPY',actor:'admin'}]})};
      assert.equal(url,'/api/admin/symbols');
      if(options.method==='POST'){
        assert.deepEqual(JSON.parse(options.body),{symbol:'NSTAR',enabled:false});
        adminRows=[{symbol:'USDJPY',provider:'legacy',enabled:true},{symbol:'NSTAR',provider:'simulation',enabled:false},{symbol:'BTCUSDT',provider:'legacy',enabled:true}];
      }
      return {json:async()=>({ok:true,rows:adminRows})};
    }});
  vm.runInContext(fs.readFileSync('static/admin.js','utf8'),admin);
  await vm.runInContext('refresh()',admin);
  assert.deepEqual(adminRequests,['/api/admin/users','/api/admin/deposits','/api/admin/symbols']);
  assert.equal(adminElements.symbols.children.length,1);
  assert.equal(adminElements.symbols.children[0].children[0].textContent,'NSTAR');
  assert.equal(adminElements.users.children.length,1,'NSTAR-only admin refresh did not load accounts');
  assert.equal(adminElements['fund-button'].disabled,false);
  assert.equal(adminElements.audit.children[0].children[2].textContent,'USDJPY','legacy audit disappeared');
  const toggle=adminElements.symbols.children[0].children[2].children[0];
  assert.equal(toggle.checked,true);assert.equal(toggle.attributes['aria-label'],'启用 NSTAR');
  toggle.checked=false;await toggle.onchange();
  assert.equal(adminElements.symbols.children.length,1,'admin mixed response exposed legacy controls');
  assert.equal(adminElements.symbols.children[0].children[0].textContent,'NSTAR');
  assert.equal(adminElements.symbols.children[0].children[2].children[0].checked,false);
  assert.match(adminElements.message.textContent,/NSTAR 已禁用新开仓/);
  vm.runInContext("renderSymbols([{symbol:'USDJPY',enabled:true}])",admin);
  assert.equal(adminElements.symbols.children.length,0,'legacy-only admin response exposed a switch');
  assert.doesNotMatch(fs.readFileSync('static/admin.html','utf8'),/FXSIM|外汇主源/);
  // Beijing display shift: the chart sees +8h times, and queued sim bars round-trip unchanged.
  run('CHART_SHIFT=8*3600');
  assert.deepEqual(JSON.parse(run('JSON.stringify(toBar([1793203200000,1,2,0.5,1.5]))')),{time:1793203200+8*3600,open:1,high:2,low:.5,close:1.5});
  run("cur={sym:'NSTAR',itv:'1d'};lastSym=cur.sym;lastItv=cur.itv;kLoaded=true;simBars=[];simStamp=0;pendingSim.clear();pendingSimStamp=0");displayed=[];
  run('queueSim([[1793203200000,1,2,0.5,1.5]],1)');
  assert.equal(displayed.at(-1).time,1793203200+8*3600,'queued bar lost or double-shifted the Beijing offset');
  run('CHART_SHIFT=0');
  // Order book: fixed grid, resting player orders add to their level, changed levels flash.
  run("cur={sym:'NSTAR',itv:'15m'};pricePrecisions.set('NSTAR',2);market={depth:50000,amm_width:.08,anchor:100,fair:100,resting:{bids:[[99.9,500]],asks:[]}}");
  run('bookGrid=null;renderBook(100)');
  const bidRows=elements['book-bids'].children,askRows=elements['book-asks'].children;
  const size=node=>Number(node.children[1].textContent.replace(/,/g,''));
  assert.equal(bidRows[0].children[0].textContent,'99.95');assert.equal(bidRows[1].children[0].textContent,'99.90');
  assert.ok(size(bidRows[1])>500&&size(bidRows[1])<560,'resting bid missing from its level');
  assert.ok(size(bidRows[0])<60,'resting bid leaked into the wrong level');
  const flashBefore=bidRows[1].className,askBefore=askRows.map(n=>n.className);
  run("market={...market,resting:{bids:[[99.9,600]],asks:[]}};renderBook(100)");
  assert.notEqual(bidRows[1].className,flashBefore,'changed level did not flash');
  assert.match(bidRows[1].className,/flash-[ab]/);
  assert.deepEqual(askRows.map(n=>n.className),askBefore,'unchanged levels flashed');
  // A small move keeps the 0.05 grid: 100.00 drops below the mid and becomes the top bid.
  run('renderBook(100.01)');
  assert.deepEqual(bidRows.slice(0,3).map(n=>n.children[0].textContent),['100.00','99.95','99.90']);
  // Spacing does not flip as the price hovers around 100, but follows a doubled price.
  run('bookGrid=null');assert.equal(run('bookStep(99.5)'),.05);assert.equal(run('bookStep(100.9)'),.05);
  assert.equal(run('bookStep(99.5)'),.05);assert.equal(run('bookStep(205)'),.2);
  run("market=null");
  console.log('PASS: legacy OHLC/history/safety/trading guards; 50 ticks zero setData; bounded bars; DOM/price-line/book/chat reuse; hidden WS zero DOM; rAF/2Hz/view commands; Top50/personal rank/pagination; init singleflight/abort/parallel K; local v5/SRI fallback; prev_close; expiry/heartbeat/backoff');
})().catch(error => { console.error(error); process.exitCode = 1; });

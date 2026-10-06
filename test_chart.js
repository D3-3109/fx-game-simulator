// Run through test_fx_sim.py: embedded browser script arrives on stdin.
const assert = require('node:assert/strict');
const vm = require('node:vm');
const script = require('node:fs').readFileSync(0, 'utf8');
const elements = {};
let autoScale = false, sets = 0, updates = [], removed = [], ranges = [];
let response = [], displayed = [], now = Date.now(), requests = [], responseStamp = 0;
const series = {
  setData(data) { sets++; displayed = data.map(bar => ({...bar})); },
  update(bar) {
    const latest = displayed.at(-1);
    if(latest && bar.time < latest.time) throw Error('Cannot update older candle');
    if(latest && bar.time === latest.time) displayed[displayed.length-1] = {...bar};
    else displayed.push({...bar});
    updates.push(bar);
  },
  removePriceLine(line) { removed.push(line); },
  createPriceLine(options) { return options; },
};
const scale = { applyOptions(options) { autoScale = options.autoScale; } };
const chart = {
  priceScale(id) { assert.equal(id, 'right'); return scale; },
  timeScale() { return {setVisibleLogicalRange(range) { ranges.push(range); }}; },
};
const context = vm.createContext({
  document: {getElementById(id) {
    return elements[id] ||= {style: {setProperty(key,value){this[key]=value;}}, dataset: {}, hidden:false, value: '', textContent: '', innerHTML: '',
      clientHeight:id==='workspace'?600:400,clientWidth:1000,offsetHeight:10,scrollHeight:0,scrollTop:0,attributes:{},
      setAttribute(key,value) { this.attributes[key]=String(value); },
      removeAttribute(key) { delete this.attributes[key]; },
      setPointerCapture(id) { this.capture=id; },
      options:[],add(option) { this.options.push(option); },replaceChildren(){this.options=[];},
      querySelector() { return this; }};
  }},
  localStorage: {},
  location: {host:'localhost:8000'},
  WebSocket: class {},
  AbortSignal,
  Date: class extends Date {static now(){return now;} },
  Option: class {constructor(text,value){this.text=text;this.value=value??text;} },
  fetch: async url => { requests.push(url); return {json: async () => ({ok: true, klines: response, updated_at: responseStamp})}; },
  setTimeout() {},
  console,
});
vm.runInContext(script, context);
Object.assign(context, {testSeries: series, testChart: chart});
vm.runInContext('series=testSeries;chart=testChart;', context);
const run = code => vm.runInContext(code, context);
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
      document:{getElementById(id){return loaderElements[id]||={...context.document.getElementById(id),
          style:{setProperty(key,value){this[key]=value;}},attributes:{},dataset:{},options:[],value:''};},
        head:{append(node){scripts.push(node);}},
        createElement(tag){assert.equal(tag,'script');return {remove(){this.removed=true;}};}},
      setTimeout(callback,ms){assert.equal(ms,5000);timers.set(++clock,callback);return clock;},
      clearTimeout(id){timers.delete(id);},
    });
    vm.runInContext(script,sandbox);
    return {sandbox,scripts,timers,load:()=>vm.runInContext('loadChartLibrary()',sandbox),
      ready(){sandbox.LightweightCharts={version:()=> '5.2.1',CandlestickSeries:{type:'Candlestick'}};}};
  };
  const tick=()=>new Promise(resolve=>setImmediate(resolve));
  const cdn=makeLoader(),cdnLoad=cdn.load();
  assert.equal(cdn.load(),cdnLoad,'parallel chart requests downloaded library twice');
  assert.equal(cdn.scripts.length,1);
  assert.equal(cdn.scripts[0].src,'https://unpkg.com/lightweight-charts@5.2.1/dist/lightweight-charts.standalone.production.js');
  assert.equal(cdn.scripts[0].crossOrigin,'anonymous');
  assert.equal(cdn.scripts[0].integrity,'sha256-4hzFyqAibvML2FScULnvkmYV8qTua05IY1NHelX1mM8=');
  cdn.ready();cdn.scripts[0].onload();await cdnLoad;
  assert.equal(cdn.timers.size,0);await cdn.load();assert.equal(cdn.scripts.length,1);
  const fallback=makeLoader(),fallbackLoad=fallback.load();
  fallback.scripts[0].onerror();await tick();
  assert.equal(fallback.scripts[0].removed,true);
  assert.equal(fallback.scripts[1].src,'/assets/lightweight-charts-5.2.1.js');
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
  assert.equal(run('cur.sym'), 'USDJPY');
  assert.equal(run('itvSel.value'), '15m', 'initial selector disagrees with default');
  assert.equal(run('symSel.value'), run('cur.sym'));
  response = candle(1.1);
  await run('loadK()');
  assert.match(requests[0], /symbol=USDJPY&interval=15m(?:&|$)/, 'initial request used wrong symbol/timeframe');
  assert.equal(run('lastItv'), '15m');
  sets = 0; updates = []; ranges = [];
  assert.equal(elements['chartpane-status'].hidden, true, 'chart loader stayed after HTTP candles');
  assert.equal(elements.chartpane.attributes['aria-busy'], 'false');

  // Whitelist changes arrive over WS. A disabled selected chart is viewable, not tradable.
  assert.equal(elements['trade-buy'].disabled,true,'trade enabled before whitelist loaded');
  context.symbolSettings={version:10,rows:[{symbol:'USDJPY',enabled:true},{symbol:'FXSIM',enabled:true}]};
  run('applySymbols(symbolSettings)');
  assert.equal(elements['trade-buy'].disabled,false);
  context.symbolSettings={version:11,rows:[{symbol:'USDJPY',enabled:false},{symbol:'FXSIM',enabled:true}]};
  context.symbolPacket=JSON.stringify({type:'symbols',...context.symbolSettings});
  run('wsConnect();ws.onmessage({data:symbolPacket})');
  assert.equal(elements['trade-buy'].disabled,true);assert.equal(elements['trade-sell'].disabled,true);
  assert.equal(run('cur.sym'),'USDJPY','disable silently changed selected pair');
  const disabledOption=elements.sym.options.find(option=>option.value==='USDJPY');
  assert.equal(disabledOption.disabled,true);assert.match(disabledOption.text,/仅查看/);
  run('applySymbols({version:9,rows:[{symbol:"USDJPY",enabled:true}]})');
  assert.equal(elements['trade-buy'].disabled,true,'stale configuration re-enabled trading');
  const beforeOrders=requests.length;await run("trade('buy')");
  assert.equal(requests.length,beforeOrders,'disabled order still submitted');
  let resolveSymbols;
  const originalFetch=context.fetch;context.fetch=()=>new Promise(resolve=>{resolveSymbols=resolve;});
  const loadingSymbols=run('loadSymbols()');
  run('applySymbols({version:12,rows:[{symbol:"FXSIM",enabled:true}]})');
  resolveSymbols({json:async()=>({ok:true,version:10,rows:[{symbol:'USDJPY',enabled:true}]})});await loadingSymbols;
  assert.equal(elements['trade-buy'].disabled,true,'late HTTP enabled disabled symbol');
  context.fetch=async()=>({json:async()=>({ok:false,err:'禁用'})});
  elements.sym.value='FXSIM';await run('symSel.onchange()');
  assert.equal(run('cur.sym'),'USDJPY','failed switch changed chart pair');
  assert.equal(elements.sym.value,'USDJPY','failed switch did not restore selector');
  run('applySymbols({version:13,rows:[]})');
  assert.equal(elements.sym.disabled,true);assert.match(elements['symbol-status'].textContent,/暂无启用/);
  context.fetch=originalFetch;

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
  context.statePacket=JSON.stringify({ok:true,type:'state',symbol:'FXSIM',price:1.1,balance:10000,equity:10000,used:0,leverage:100,margin_level:null,positions:[]});
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
  run('symSynced=true;');
  context.fetch=async()=>({json:async()=>JSON.parse(context.statePacket)});await run('refresh()');
  assert.equal(elements['positions-status'].hidden,true,'HTTP state success stayed loading');
  context.fetch=normalFetch;

  // Leverage edit is a draft: live state must not reset input, even while save is pending.
  context.levState={...JSON.parse(context.statePacket),leverage:100,positions:[]};
  run('levDirty=false;render(levState)');
  assert.equal(elements.lev.value,100);
  run("$('lev').value='500';levDirty=true;render(levState)");
  assert.equal(elements.lev.value,'500','live state overwrote unsaved higher leverage');
  let finishLev;
  context.fetch=(url,options)=>{
    assert.equal(url,'/api/leverage');assert.equal(JSON.parse(options.body).leverage,500);
    return new Promise(resolve=>{finishLev=resolve;});
  };
  const saving=run('setLev()');
  assert.equal(elements.lev.disabled,true);assert.equal(elements['lev-apply'].disabled,true);
  run('render(levState)');assert.equal(elements.lev.value,'500','state overwrote in-flight save');
  context.levState.leverage=500;
  context.fetch=async()=>({json:async()=>context.levState});
  finishLev({status:200,json:async()=>({ok:true})});await saving;
  assert.equal(run('accountLeverage'),500);assert.equal(run('levDirty'),false);
  assert.equal(elements.lev.disabled,false);assert.match(elements['lev-hint'].textContent,/500/);
  context.fetch=async()=>({status:400,json:async()=>({ok:false,err:'先平仓'})});
  run("$('lev').value='1000';levDirty=true");await run('setLev()');
  assert.equal(elements.lev.value,500,'rejected leverage still appeared applied');
  const blocked=[];context.fetch=async url=>{blocked.push(url);throw Error('must not request');};
  run("$('lev').value='1.5';levDirty=true");await run('setLev()');
  assert.equal(blocked.length,0);
  context.levState.positions=[{id:1,side:'long',lots:.1,sym:'FXSIM',entry:1.1,now:1.1,pnl:0,margin:11}];
  run('render(levState)');assert.equal(elements.lev.disabled,true);assert.equal(elements['lev-apply'].disabled,true);
  await run('setLev()');assert.equal(blocked.length,0,'in-position leverage changed');
  context.levState.positions=[];run('render(levState)');context.fetch=normalFetch;

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
  const ledger=[{id:10,time:1800000000000,kind:'stopout',position_id:9,symbol:'<img onerror="bad">',side:'long',lots:.1,entry:1,price:.9998,pnl:-2,balance:48,margin_level:.49}];
  context.fetch=async()=>({json:async()=>({ok:true,rows:ledger})});
  await run('loadHistory()');
  assert.equal(elements['history-status'].hidden,true);
  assert.match(elements.historyT.innerHTML,/强平/);assert.match(elements.historyT.innerHTML,/-\$2\.00/);
  assert.match(elements.historyT.innerHTML,/&lt;img/);assert.doesNotMatch(elements.historyT.innerHTML,/<img/);
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
  assert.match(elements.acct.innerHTML,/保证金比例.*0%/);assert.match(elements.acct.innerHTML,/强平线 50%/);
  assert.match(elements.acct.innerHTML,/\$10000\.00/);
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
    document:{getElementById(id){return otherElements[id]||=( {...elements.splitter,style:{setProperty(key,value){this[key]=value;}},attributes:{},clientWidth:1000,clientHeight:600});}}});
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
      return {subscribeCrosshairMove(){},addSeries(definition,options){assert.equal(definition.type,'Candlestick');assert.equal(options.wickUpColor,'#26a69a');return series;},applyOptions(options){applied=options;}};
    }},
    ResizeObserver:class {constructor(callback){this.callback=callback;}observe(el){observe=this.callback;assert.equal(el,elements.cv);}},
  });
  run('chart=null;initChart()');
  elements.cv.clientHeight=250;elements.cv.clientWidth=700;observe();
  assert.equal(applied.height,250);assert.equal(applied.width,700);
  elements.cv.clientHeight=0;observe();assert.equal(applied.height,250, 'hidden chart was resized to zero');
  run('chart=testChart');

  // Switching clears former symbol's position lines immediately.
  removed=[];
  run("loadK=()=>{};refresh=()=>{};api=async()=>({ok:true});cur.sym='BTCUSDT';priceLines=[{price:60000}];curPos=[{sym:'BTCUSDT',entry:60000,side:'long',lots:.1}];symSel.value='FXSIM';itvSel.value='5m';");
  await run('symSel.onchange()');
  assert.equal(removed.length, 1, 'old position line survives symbol switch');
  assert.equal(run('priceLines.length'), 0);
  console.log('PASS: switch scales, stale-state guards, periodic zoom, OHLC/wicks, authoritative simulated spikes, closed candles, Ctrl+R, panel splitter, chart sizing and independent loading states, realized history, stopout notices, margin labels, leverage drafts, resizable sidebars/history and v5 CDN/SRI/fallback/timeout/retry');
})().catch(error => { console.error(error); process.exitCode = 1; });

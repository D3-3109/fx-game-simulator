
const $=id=>document.getElementById(id);
const amountFormat=new Intl.NumberFormat('en-US',{minimumFractionDigits:2,maximumFractionDigits:2});
const money=value=>`${value<0?'-':''}$${amountFormat.format(Math.abs(value))}`;
const pricePrecisions=new Map();
function decimalPlaces(value){
 const [base,exponent='0']=String(value).toLowerCase().split('e');
 return Math.max(0,(base.split('.')[1]||'').length-Number(exponent));
}
function priceFormat(symbol){
 const precision=pricePrecisions.get(symbol)??0;
 return {type:'price',precision,minMove:10**-precision};
}
// Simulated books produce unrounded fills; pin their display precision like an exchange tick.
const FIXED_PRECISION={NSTAR:2};
function observePrices(symbol,values){
 const precision=FIXED_PRECISION[symbol]??Math.min(10,Math.max(pricePrecisions.get(symbol)??0,...values.filter(Number.isFinite).map(decimalPlaces)));
 if(pricePrecisions.get(symbol)===precision)return;
 pricePrecisions.set(symbol,precision);
 if(series&&symbol===cur.sym)series.applyOptions({priceFormat:priceFormat(symbol)});
}
// Exchange-style display names; the stored symbol id stays NSTAR / FXSIM.
const pairName=s=>s==='NSTAR'?'NSTAR/USDT':s==='FXSIM'?'FXSIM/USD':/USDT$/.test(s)?s.replace(/USDT$/,'/USDT'):s;
const priceText=(value,symbol=cur.sym)=>Number.isFinite(value)?value.toFixed(pricePrecisions.get(symbol)??decimalPlaces(value)):'—';
let TOKEN=localStorage.fx_token||'',ME='',cur={sym:'NSTAR',itv:'15m'};
let chart=null,series=null,lastBar=null,priceLines=new Map(),curPos=[],lastSym=null,lastItv=null;
let levDirty=false,levSaving=false,accountLeverage=10,orderBusy=false,authBusy=false;
let quoteSymbol='',quoteLast=null,quoteReady=false,quoteTimer=null,quoteBid=NaN,quoteAsk=NaN;
let mode='perp',spotSide='buy',spotType='market',lastState=null,market=null,tpslId=0,lotsFor='',otype='market';
const quoteReferences=new Map();
/* One visual queue: network ingestion/safety stays live, DOM work waits for a visible frame. */
let lowPower=false,frame=null,frameTimer=null,lastFrame=-Infinity;
try{lowPower=localStorage.fx_low_power==='true'}catch(_){}
const visuals=new Map(),flights=new Map();
const visible=id=>!document.hidden&&!$(id)?.closest('[hidden]');
const paneVisible=(attr,key)=>!document.hidden&&!document.querySelector(`[data-${attr}-pane="${key}"]`)?.hidden;
function singleflight(key,work){
 if(flights.has(key))return flights.get(key);
 const promise=work().finally(()=>{if(flights.get(key)===promise)flights.delete(key)});
 flights.set(key,promise);return promise;
}
function scheduleVisual(key,paint){visuals.set(key,paint);requestRender()}
function requestRender(){
 if(document.hidden||frame!==null||frameTimer!==null)return;
 const wait=lowPower?Math.max(0,500-(Date.now()-lastFrame)):0;
 if(wait){frameTimer=setTimeout(()=>{frameTimer=null;requestRender()},wait);return}
 frame=requestAnimationFrame(()=>{
  frame=null;if(document.hidden)return;lastFrame=Date.now();
  const pending=[...visuals];visuals.clear();
  for(const [key,paint] of pending)if(paint()===false&&!visuals.has(key))visuals.set(key,paint);
 });
}
function htmlIfChanged(node,html){if(node.innerHTML!==html)node.innerHTML=html}
/* For nodes written only through here: compare with the last string written instead of
   reading innerHTML back, which serializes the whole subtree on every call. */
const painted=new WeakMap();
function htmlOnce(node,html){if(painted.get(node)!==html){painted.set(node,html);node.innerHTML=html}}
/* Per-tick paints only touch nodes whose text, class or flag actually changed: an
   unchanged write still invalidates style and layout for that subtree. */
function setText(node,text){text=String(text);if(node.textContent!==text)node.textContent=text}
function setClass(node,name){if(node.className!==name)node.className=name}
function setFlag(node,key,value){if(node[key]!==value)node[key]=value}
const signatures=new Map();
function changed(key,value){const signature=JSON.stringify(value);if(signatures.get(key)===signature)return false;signatures.set(key,signature);return true}
function tableVisible(id,attr,key){return visible(id)&&paneVisible(attr,key)}
let latestQuote=null,quoteStamp=0,quoteExpires=Infinity;
const usableQuote=()=>quoteReady&&Date.now()<=quoteExpires;
function acceptQuote(price,symbol,bid,ask,valid=true,at){
 if(symbol!==cur.sym||Number.isFinite(at)&&symbol===latestQuote?.symbol&&at<quoteStamp)return;
 quoteStamp=Number.isFinite(at)?at:0;
 latestQuote={price,symbol,bid,ask,valid,at};
 // Never wait for rAF to revoke an expired/invalid trading quote.
 quoteReady=valid&&Number.isFinite(bid)&&Number.isFinite(ask)&&bid>0&&ask>=bid&&(!Number.isFinite(at)||Date.now()-at<=30000);
 quoteBid=bid;quoteAsk=ask;quoteExpires=Number.isFinite(at)?at+30000:Infinity;
 scheduleVisual('quote',()=>{const q=latestQuote;renderQuote(q.price,q.symbol,q.bid,q.ask,q.valid,q.at);updateLast(q.price);renderBook(q.price)});
}
function renderQuote(price,symbol,bid,ask,valid=true,at){
 if(document.hidden){acceptQuote(price,symbol,bid,ask,valid,at);return}
 if(symbol!==cur.sym)return;
 const fx=symbol!=='FXSIM'&&!symbol.endsWith('USDT');
 observePrices(symbol,fx?[price,bid,ask]:[price]);
 const sideText=value=>fx||symbol in FIXED_PRECISION?priceText(value,symbol):value.toFixed(Math.max(pricePrecisions.get(symbol)??0,4));
 clearTimeout(quoteTimer);
 quoteReady=valid&&Number.isFinite(bid)&&Number.isFinite(ask)&&bid>0&&ask>=bid&&(!Number.isFinite(at)||Date.now()-at<=30000);
 quoteBid=bid;quoteAsk=ask;quoteExpires=Number.isFinite(at)?at+30000:Infinity;
 setText($('buy-price'),quoteReady?sideText(ask):'—');
 setText($('sell-price'),quoteReady?sideText(bid):'—');
 if(quoteReady&&Number.isFinite(at))quoteTimer=setTimeout(()=>{
  quoteReady=false;
  scheduleVisual('quote-expiry',()=>{if(!quoteReady){$('buy-price').textContent=$('sell-price').textContent='—';syncOrderControls()}});
 },Math.max(0,at+30000-Date.now()));
 syncOrderControls();
 const node=$('price');
 if(quoteSymbol!==symbol){quoteSymbol=symbol;quoteLast=null;delete node.dataset.direction}
 if(Number.isFinite(price)&&quoteLast!==null&&price!==quoteLast)node.dataset.direction=price>quoteLast?'up':'down';
 const formatted=priceText(price,symbol),title=`${formatted} | ${symbol==='NSTAR'?pairName(symbol):symbol}`;
 if(document.title!==title)document.title=title;
 const [whole,decimal]=formatted.split('.');
 htmlOnce(node,Number.isFinite(price)?Number(whole).toLocaleString('en-US')+(decimal?`<span class="quote-decimal">.${decimal}</span>`:''):'—');
 setText($('quote-unit'),symbol==='NSTAR'||symbol.endsWith('USDT')?'USDT':isSim(symbol)||STOCKS.includes(symbol)?'USD':symbol.slice(-3));
 quoteLast=Number.isFinite(price)?price:null;
 renderQuoteChange();
 if(quoteLast!==null)loadQuoteReference(symbol);
}
function renderQuoteChange(){
 const reference=quoteReferences.get(quoteSymbol)?.close,node=$('quote-change');
 if(quoteSymbol!==cur.sym||quoteLast===null||!Number.isFinite(reference)||reference<=0){setText(node,'');setClass(node,'quote-change');return}
 const delta=Number((quoteLast-reference).toFixed(pricePrecisions.get(quoteSymbol)??decimalPlaces(quoteLast))),sign=delta>0?'+':'';
 setText(node,`${sign}${priceText(delta,quoteSymbol)}  ${sign}${(delta/reference*100).toFixed(2)}%`);
 setClass(node,'quote-change '+(delta===0?'muted':delta>0?'green':'red'));
}
async function loadQuoteReference(symbol){
 if(document.hidden||symbol==='NSTAR')return; // NSTAR uses the quote's market baseline, not a second K request.
 const previous=quoteReferences.get(symbol);
 if(previous&&(previous.pending||Date.now()-previous.at<60000))return;
 const reference={at:Date.now(),pending:true};quoteReferences.set(symbol,reference);
 try{
  const r=await fetch(`/api/klines?symbol=${symbol}&interval=1d&limit=2`,{signal:AbortSignal.timeout(20000)}),j=await r.json();
  if(!j.ok||!Array.isArray(j.klines)||j.klines.length<2)return;
  const close=j.klines.at(-2)[4];if(Number.isFinite(close)&&close>0)reference.close=close;
 }catch(_){}  // 没有前日收盘数据时留空，不用当前价或开盘价冒充。
 finally{reference.pending=false;if(symbol===cur.sym)scheduleVisual('quote-change',renderQuoteChange)}
}
function panelStatus(id,state='ready',text=''){
 if(document.hidden){scheduleVisual('status-'+id,()=>panelStatus(id,state,text));return}
 const status=$(id+'-status');status.hidden=state==='ready';
 status.dataset.state=state;status.querySelector('.status-text').textContent=text;
 $(id).setAttribute('aria-busy',state==='loading');
}

/* ---------- auth ---------- */
async function doAuth(action){
 if(authBusy||!$('auth-form').reportValidity())return;
 authBusy=true;$('auth-login').disabled=$('auth-register').disabled=true;
 $('autherr').textContent='';
 try{const r=await fetch('/api/auth',{method:'POST',headers:{'Content-Type':'application/json'},
  body:JSON.stringify({action,name:$('f-name').value.trim(),pwd:$('f-pwd').value})});
  const j=await r.json();if(!j.ok)throw j.err||'失败';
  TOKEN=j.token;localStorage.fx_token=TOKEN;boot();
 }catch(e){$('autherr').textContent=String(e)}
 finally{authBusy=false;$('auth-login').disabled=$('auth-register').disabled=false}}

function logout(){localStorage.removeItem('fx_token');location.reload()}
async function me(){
 try{const r=await fetch('/api/me',{headers:authH()});const j=await r.json();
  if(!j.ok)return false;ME=j.name;$('who').textContent=ME;return true}catch(e){return false}}
function authH(h){h=h||{};h['Authorization']='Bearer '+TOKEN;return h}

/* 可调面板：原生指针捕获，无过渡动画，拖动时1:1跟随。 */
const splitter=$('splitter'),workspace=$('workspace');
let panelRatio=.68,splitDrag=null;
try{const saved=Number(localStorage.fx_panel_ratio);if(saved>=.15&&saved<=.9)panelRatio=saved}catch(_){}
function setPanelRatio(ratio){
 panelRatio=Math.max(.15,Math.min(.9,ratio));
 workspace.style.gridTemplateRows=`minmax(120px,${panelRatio}fr) 10px minmax(170px,${1-panelRatio}fr)`;
 splitter.setAttribute('aria-valuenow',Math.round(panelRatio*100));
}
function savePanelRatio(){try{localStorage.fx_panel_ratio=panelRatio}catch(_){}}
setPanelRatio(panelRatio);
splitter.onpointerdown=e=>{
 if(e.button!==0||splitDrag)return;
 const total=workspace.clientHeight-splitter.offsetHeight;if(total<=0)return;
 e.preventDefault();splitter.setPointerCapture(e.pointerId);
 splitDrag={id:e.pointerId,y:e.clientY,height:$('chartpane').clientHeight,total};
 splitter.setAttribute('data-dragging','');
};
splitter.onpointermove=e=>{
 if(!splitDrag||e.pointerId!==splitDrag.id)return;
 const d=splitDrag,height=Math.max(120,Math.min(d.total-170,d.height+e.clientY-d.y));
 setPanelRatio(height/d.total);
};
function endSplit(e){
 if(!splitDrag||e.pointerId!==splitDrag.id)return;
 splitDrag=null;splitter.removeAttribute('data-dragging');savePanelRatio();
}
splitter.onpointerup=splitter.onpointercancel=splitter.onlostpointercapture=endSplit;
splitter.ondblclick=()=>{setPanelRatio(.68);savePanelRatio()};
splitter.onkeydown=e=>{
 const next={ArrowUp:panelRatio-.03,ArrowDown:panelRatio+.03,Home:.15,End:.9}[e.key];
 if(next===undefined)return;e.preventDefault();setPanelRatio(next);savePanelRatio();
};

/* 侧栏和成交记录：复用原生指针拖动，尺寸只保存在本机。 */
let sideWidths={left:240,right:264},historyRatio=.6,chatRatio=.5;
try{
 const saved=JSON.parse(localStorage.fx_sidebar_widths||'null');
 for(const key of ['left','right'])if(saved&&Number.isFinite(saved[key])&&saved[key]>=160&&saved[key]<=520)sideWidths[key]=saved[key];
 const ratio=Number(localStorage.fx_history_ratio);if(ratio>=.15&&ratio<=.85)historyRatio=ratio;
 const chat=Number(localStorage.fx_chat_ratio);if(chat>=.15&&chat<=.85)chatRatio=chat;
}catch(_){}
function resizeSides(){
 if(document.hidden)return;
 const wrap=$('wrap');if(wrap.clientWidth<=800)return;
 const budget=wrap.clientWidth-340;
 sideWidths.left=Math.max(160,Math.min(520,sideWidths.left,budget-160));
 sideWidths.right=Math.max(160,Math.min(520,sideWidths.right,budget-sideWidths.left));
 wrap.style.setProperty('--trade-width',sideWidths.left+'px');wrap.style.setProperty('--rank-width',sideWidths.right+'px');
 $('left-splitter').setAttribute('aria-valuenow',Math.round(sideWidths.left));
 $('right-splitter').setAttribute('aria-valuenow',Math.round(sideWidths.right));
}
function setHistoryRatio(ratio){
 historyRatio=Math.max(.15,Math.min(.85,ratio));
 $('lowerpanels').style.gridTemplateRows=`minmax(80px,${historyRatio}fr) 10px minmax(80px,${1-historyRatio}fr)`;
 $('history-splitter').setAttribute('aria-valuenow',Math.round(historyRatio*100));
}
function bindPaneResize(id,axis,read,write,save,restore,direction=1){
 const handle=$(id),coord=axis==='x'?'clientX':'clientY';let drag=null;
 handle.onpointerdown=e=>{
  if(e.button!==0||drag)return;e.preventDefault();handle.setPointerCapture(e.pointerId);
  drag={id:e.pointerId,start:e[coord],size:read()};handle.setAttribute('data-dragging','');
 };
 handle.onpointermove=e=>{if(drag&&e.pointerId===drag.id)write(drag.size+(e[coord]-drag.start)*direction)};
 const end=e=>{if(!drag||e.pointerId!==drag.id)return;drag=null;handle.removeAttribute('data-dragging');save()};
 handle.onpointerup=handle.onpointercancel=handle.onlostpointercapture=end;
 handle.ondblclick=()=>{restore();save()};
 handle.onkeydown=e=>{
  const delta=axis==='x'?{ArrowLeft:-16,ArrowRight:16}[e.key]:{ArrowUp:-16,ArrowDown:16}[e.key];
  if(delta===undefined)return;e.preventDefault();write(read()+delta*direction);save();
 };
}
const saveSides=()=>{try{localStorage.fx_sidebar_widths=JSON.stringify(sideWidths)}catch(_){}};
for(const [key,id,sign,width] of [['left','left-splitter',1,240],['right','right-splitter',-1,264]]){
 bindPaneResize(id,'x',()=>$(key==='left'?'tradebar':'rightbar').clientWidth,
  value=>{sideWidths[key]=Math.max(160,Math.min(520,value,$('wrap').clientWidth-340-sideWidths[key==='left'?'right':'left']));resizeSides()},
  saveSides,()=>{sideWidths[key]=width;resizeSides()},sign);
}
bindPaneResize('history-splitter','y',()=>$('positions').clientHeight,
 value=>{const total=$('lowerpanels').clientHeight-10;if(total>160)setHistoryRatio(Math.max(80,Math.min(total-80,value))/total)},
 ()=>{try{localStorage.fx_history_ratio=historyRatio}catch(_){}},()=>setHistoryRatio(.6));
function setChatRatio(ratio){
 chatRatio=Math.max(.15,Math.min(.85,ratio));
 $('rightbar').style.gridTemplateRows=`minmax(120px,${chatRatio}fr) 10px minmax(160px,${1-chatRatio}fr)`;
 $('chat-splitter').setAttribute('aria-valuenow',Math.round(chatRatio*100));
}
bindPaneResize('chat-splitter','y',()=>$('lbbar').clientHeight,
 value=>{const total=$('rightbar').clientHeight-10;if(total>280)setChatRatio(Math.max(120,Math.min(total-160,value))/total)},
 ()=>{try{localStorage.fx_chat_ratio=chatRatio}catch(_){}},()=>setChatRatio(.5));
setHistoryRatio(historyRatio);setChatRatio(chatRatio);resizeSides();

/* Local pinned 5.2.1 first, original CDN + SRI as a bounded fallback. */
let chartLibraryPromise=null;
function loadChartLibrary(){
 const ready=()=>typeof LightweightCharts!=='undefined'&&typeof LightweightCharts.version==='function'&&LightweightCharts.version()==='5.2.1'&&LightweightCharts.CandlestickSeries;
 if(ready())return Promise.resolve();
 if(!chartLibraryPromise)chartLibraryPromise=(async()=>{
  const load=src=>new Promise((resolve,reject)=>{
   const script=document.createElement('script');
   script.src=src;script.crossOrigin='anonymous';
   script.integrity='sha256-4hzFyqAibvML2FScULnvkmYV8qTua05IY1NHelX1mM8=';
   const finish=error=>{
    clearTimeout(timer);script.onload=script.onerror=null;
    if(error){script.remove();reject(error)}else resolve();
   };
   const timer=setTimeout(()=>finish(Error('图表组件加载超时')),5000);
   script.onload=()=>finish(ready()?null:Error('图表组件版本错误'));
   script.onerror=()=>finish(Error('图表组件加载失败'));
   document.head.append(script);
  });
  try{await load('/assets/lightweight-charts-5.2.1.js')}
  catch(_){await load('https://unpkg.com/lightweight-charts@5.2.1/dist/lightweight-charts.standalone.production.js')}
 })().catch(error=>{chartLibraryPromise=null;throw error});
 return chartLibraryPromise;
}

/* chart / trading 同前 */
const UP='#25a750',DOWN='#ca3f64';
function initChart(){
 if(chart||typeof LightweightCharts==='undefined')return;
 const cw=$('cv').clientWidth||innerWidth-24;
 chart=LightweightCharts.createChart($('cv'),{width:cw,height:$('cv').clientHeight||160,
  // The library's NOTICE requires attribution; the toolbar credit link replaces the logo.
  layout:{attributionLogo:false,background:{type:'solid',color:'#000000'},textColor:'#8a8a8a',fontSize:11,fontFamily:'Noto Sans, PingFang SC, Noto Sans SC, sans-serif'},
  grid:{vertLines:{color:'#161616'},horzLines:{color:'#161616'}},crosshair:{mode:0},
  timeScale:{borderColor:'#232323',timeVisible:true,secondsVisible:false},
  rightPriceScale:{borderColor:'#232323'}});
 chart.subscribeCrosshairMove(param=>{
  const p=param.seriesData&&param.seriesData.get(series);
  $('px').textContent=p?`开 ${priceText(p.open)} 高 ${priceText(p.high)} 低 ${priceText(p.low)} 收 ${priceText(p.close)}`:'';});
 series=chart.addSeries(LightweightCharts.CandlestickSeries,{upColor:UP,downColor:DOWN,borderVisible:false,
  wickUpColor:UP,wickDownColor:DOWN,
  priceFormat:priceFormat(cur.sym)});
 new ResizeObserver(()=>scheduleVisual('chart-size',()=>{
  const cv=$('cv');if(cv.clientWidth&&cv.clientHeight)chart.applyOptions({width:cv.clientWidth,height:cv.clientHeight});
 })).observe($('cv'));
}
function drawPosLines(){
 if(!series||!visible('chartpane'))return;
 const keep=new Set();
 for(const p of curPos.filter(p=>(p.sym||lastState?.symbol)===cur.sym)){
  const id=p.id,options={price:p.entry,color:p.side=='long'?UP:DOWN,lineWidth:1,lineStyle:2,
   axisLabelVisible:true,title:(p.side=='long'?'多 ':'空 ')+p.lots},signature=JSON.stringify(options);
  keep.add(id);const old=priceLines.get(id);
  if(!old)priceLines.set(id,{line:series.createPriceLine(options),signature});
  else if(old.signature!==signature){old.line.applyOptions(options);old.signature=signature}
 }
 for(const [id,{line}] of priceLines)if(!keep.has(id)){series.removePriceLine(line);priceLines.delete(id)}
}
const STOCKS=['AAPL','TSLA','NVDA','MSFT','AMZN','GOOGL','META','AMD'];
const symSel=$('sym');
const symbolLabel=s=>s==='NSTAR'?'NSTAR/USDT · 北极星币':s==='FXSIM'?'FXSIM · 模拟币':pairName(s);
function syncSecondOption(){
 const fx=cur.sym==='FXSIM'||cur.sym==='NSTAR';
 if(!fx&&cur.itv==='1s')cur.itv='1m';
 $('itv-quick').innerHTML=(fx?['1s','1m','5m','15m','30m','1h','4h','1d','1w']:['1m','5m','15m','30m','1h','4h','1d','1w']).map(i=>
  `<button type="button" class="itv" aria-pressed="${i===cur.itv}" onclick="pickItv('${i}')">${i}</button>`).join('');
}
function pickItv(i){
 if(i===cur.itv)return;
 cur.itv=i;lastBar=null;kLoaded=false;pendingSim.clear();syncSecondOption();drawPosLines();sendView();loadK();
}
syncSecondOption();
symSel.value=cur.sym;
let tradableSymbols=null,symbolsVersion=-1;
function syncSymbolChoices(){
 const disabled=tradableSymbols!==null&&!tradableSymbols.includes('NSTAR');
 const option=new Option(symbolLabel('NSTAR')+(disabled?' · 已禁用（仅查看）':''),'NSTAR');
 option.disabled=disabled;
 symSel.replaceChildren();symSel.add(option);symSel.value='NSTAR';
 syncSecondOption();
 syncOrderControls();
}
function syncOrderControls(){
 if(document.hidden){scheduleVisual('controls',syncOrderControls);return}
 const allowed=tradableSymbols!==null&&tradableSymbols.includes(cur.sym);
 symSel.disabled=true;  // NSTAR-only：保留只读币对展示，不提供切换入口。
 setText($('order-symbol'),pairName(cur.sym)+(cur.sym==='NSTAR'?' 永续':''));
 const whole=STOCKS.includes(cur.sym),star=cur.sym==='NSTAR';
 setFlag($('lots'),'min',whole?'1':'0.01');setFlag($('lots'),'step',whole?'1':'0.01');
 // NSTAR: 1 手 = 0.01 枚 ≈ $1。
 if(lotsFor!==cur.sym){lotsFor=cur.sym;$('lots').value=star?'1000':whole?'1':'0.10'}
 syncLotPresets();renderOrderEstimate();
 const blocked=orderBusy||!allowed||!quoteReady;
 setFlag($('trade-buy'),'disabled',blocked);setFlag($('trade-sell'),'disabled',blocked);setFlag($('spot-submit'),'disabled',blocked);
 setFlag($('close-all'),'disabled',orderBusy||curPos.length===0);
 setText($('close-all'),orderBusy?'正在处理…':'全部平仓');
 // Row buttons follow orderBusy; repainted rows render with it, so quote ticks skip the query.
 if(rowButtonsBusy!==orderBusy){rowButtonsBusy=orderBusy;document.querySelectorAll('.close-position').forEach(button=>button.disabled=orderBusy)}
 setText($('symbol-status'),orderBusy?'订单提交中，请稍候…':tradableSymbols===null?'正在获取币种配置…':!allowed?
  tradableSymbols.length?'该币对已禁用，已有持仓仍可平仓':'暂无启用的币对，已有持仓仍可平仓':
  cur.sym==='NSTAR'?'NSTAR 最高 1:10，仓位越大杠杆越低 · 总持仓不限 · 偏离锚价越远流动性越厚':'');
}
let rowButtonsBusy=false;
function applySymbols(settings){
 if(settings.version<symbolsVersion)return;
 symbolsVersion=settings.version;tradableSymbols=settings.rows.filter(row=>row.symbol==='NSTAR'&&row.enabled).map(row=>row.symbol);
 scheduleVisual('symbols',syncSymbolChoices);
}
function loadSymbols(){return singleflight('symbols',async()=>{
 try{const r=await fetch('/api/symbols',{signal:AbortSignal.timeout(20000)}),j=await r.json();
  if(!j.ok)throw j.err||'币种配置不可用';applySymbols(j);
 }catch(e){scheduleVisual('symbols-error',()=>{$('symbol-status').textContent='币种配置加载失败，请刷新重试'})}
})}
syncSymbolChoices();

async function api(path,body){const r=await fetch('/api/'+path,{method:'POST',headers:authH({'Content-Type':'application/json'}),body:JSON.stringify(body||{})});
 if(r.status==401){logout();throw'auth'}const j=await r.json();if(!j.ok)msg('操作失败：'+j.err);return j}
// Mutations return the new account; only fall back to a GET when the reply lacks it.
const settle=j=>j.state?(connOk(),render(j.state)):refresh();
let messageTimer=null;
const pendingMessages=[];
function msg(t){
 if(document.hidden){pendingMessages.push(t);scheduleVisual('messages',()=>{const text=pendingMessages.splice(0).join('\n');if(text)msg(text)});return}
 clearTimeout(messageTimer);$('msg').textContent=t;messageTimer=setTimeout(()=>{if(!document.hidden)$('msg').textContent=''},6000);
}
/* 数量滑块: 100% = 服务器按真实保证金/档位/冲击成本算出的最大可开（多空取小）。 */
const fmtLots=n=>STOCKS.includes(cur.sym)||cur.sym==='NSTAR'&&n>=1?String(+n.toFixed(2)):n.toFixed(2);
/* NSTAR 预估成交：与后端 amm_shift/amm_avg 同一公式。冲击超过 5% 标红提醒，建议改用限价单。 */
const IMPACT_WARN=.05;
function ammShift(x0,u,D,W){if(!W)return x0+u/D;const c=x0+x0**3/(3*W*W)+u/D;return 2*W*Math.sinh(Math.asinh(3*c/(2*W))/3)}
function ammAvg(x0,x1,W){const d=x1-x0,g=Math.abs(d)<1e-12?1:Math.expm1(d)/d;if(!W)return g;const w2=W*W;
 return (g*(1+(x1*x1-2*x1+2)/w2)+(x1+x0-2)/w2)/(1+(x1*x1+x1*x0+x0*x0)/(3*w2))}
// 预估 NSTAR 吃单均价（不含手续费）；units 为正买负卖。其他币对按当前买卖价。
function bookFill(units){
 const mid=nstarMid(),m=market;if(!(mid>0)||!m?.depth)return NaN;
 const W=m.amm_width||0,x0=W&&m.anchor>0?Math.log(mid/m.anchor):0;
 return mid*ammAvg(x0,ammShift(x0,units,m.depth,W),W);
}
function perpFill(side,lots){
 const c=lastState?.contract??1,u=lots*c*(side==='buy'?1:-1);
 if(cur.sym!=='NSTAR')return side==='buy'?quoteAsk:quoteBid;
 const f=bookFill(u),half=(nstarMid()||0)*.0005/2;return f+(side==='buy'?half:-half);
}
function maxLots(){const s=lastState,m=s?.limits?.max_lots;return m&&s.symbol===cur.sym?m:null}
function setSlider(el,pct,enabled){
 const fill=`${Math.min(100,Math.max(0,pct)).toFixed(1)}%`;
 setFlag(el,'value',String(Math.round(pct)));setFlag(el,'disabled',!enabled);
 if(el.dataset.fill!==fill){el.dataset.fill=fill;el.style.setProperty('--fill',fill)}
}
function syncLotPresets(){
 const m=maxLots(),cap=m?Math.min(m.buy,m.sell):0,n=Number($('lots').value),star=cur.sym==='NSTAR',c=lastState?.contract??1;
 setSlider($('lots-slider'),cap>0&&n>0?n/cap*100:0,cap>0);
 const node=$('order-max');
 setText(node,!m?'—':star?`${units(m.buy*c,2)} / ${units(m.sell*c,2)} NSTAR`:`${fmtLots(m.buy)} / ${fmtLots(m.sell)} ${STOCKS.includes(cur.sym)?'股':'手'}`);
 setClass(node,m&&n>Math.max(m.buy,m.sell)?'red':'');
 const fill=$('order-fill');
 if(n>0&&quoteReady){
  const star=cur.sym==='NSTAR',fb=perpFill('buy',n),fs=perpFill('sell',n),imp=Math.max(fb/quoteAsk-1,1-fs/quoteBid);
  if(otype!=='market'){setText(fill,otype==='limit'?'按委托价或更优成交':'触发后按市价 / 委托价');setClass(fill,'')}
  else{
   setText(fill,!Number.isFinite(fb+fs)?'—':star?`${units(fb,2)} / ${units(fs,2)} · ±${(imp*100).toFixed(1)}%`:`${units(fb,5)} / ${units(fs,5)}`);
   setClass(fill,star&&imp>IMPACT_WARN?'red':'');
  }
 }else{setText(fill,'—');setClass(fill,'')}
}
function setLots(value){$('lots').value=fmtLots(Number(value));syncLotPresets();renderOrderEstimate()}
function setLotsPct(pct){
 const m=maxLots(),cap=m?Math.min(m.buy,m.sell):0;if(!(cap>0))return;
 const step=STOCKS.includes(cur.sym)?1:.01,min=STOCKS.includes(cur.sym)?1:.01;
 setLots(Math.min(cap,Math.max(min,Math.floor(cap*pct/100/step+1e-9)*step)));
}
$('lots-slider').addEventListener('input',e=>setLotsPct(Number(e.target.value)));
/* NSTAR 快讯: 传闻/消息/巨鲸，所有人可见，仅保留最近 4 条。 */
let nstarFeed=[];
function nstarEvent(e){
 nstarFeed=[...nstarFeed,e].slice(-4);
 scheduleVisual('feed',()=>{$('nstar-feed').innerHTML=nstarFeed.map(x=>`<li data-kind="${escapeHtml(x.kind)}"><time>${escapeHtml(new Date(x.time).toLocaleTimeString())}</time>${escapeHtml(x.text)}</li>`).join('')});
}
$('lots').addEventListener('input',syncLotPresets);syncLotPresets();
async function trade(side){
 if(orderBusy)return;
 if(tradableSymbols===null||!tradableSymbols.includes(cur.sym)){msg('该币对暂不可新开仓');return}
 if(!usableQuote()){quoteReady=false;syncOrderControls();msg('报价不可用，请等待更新');return}
 if(!$('lots').reportValidity())return;
 orderBusy=true;syncOrderControls();
 const lots=Number($('lots').value),tp=Number($('entry-tp').value)||0,sl=Number($('entry-sl').value)||0;
 const price=Number($('order-price').value)||0,trigger=Number($('order-trigger').value)||0;
 const word=side==='buy'?'买入':'卖出';
 try{
  if(otype==='limit'&&!(price>0))throw '请填写委托价';
  if(otype==='stop'&&!(trigger>0))throw '请填写触发价';
  const j=otype==='market'?await api('trade',{side,lots,tp,sl})
   :await api('order',{kind:otype,side,lots,price:otype==='limit'?price:trigger,limit:otype==='stop'?price:0,tp,sl});
  if(j.ok){msg(otype==='market'?`${cur.sym} ${word}已成交`:`${cur.sym} ${otype==='limit'?'限价':'条件'}${word}委托已提交，可在「委托」中查看`);await settle(j)}
 }catch(e){msg('下单失败：'+e)}
 finally{orderBusy=false;syncOrderControls()}
}
/* 委托类型: 市价 / 限价（只按委托价或更优成交，可部分成交、余量继续挂单）/ 条件单（价格穿过触发价后，按委托价挂限价；委托价留空=市价）。 */
function setOrderType(t){
 otype=t;document.querySelectorAll('[data-otype]').forEach(b=>b.setAttribute('aria-selected',b.dataset.otype===t));
 $('trigger-field').hidden=t!=='stop';$('price-field').hidden=t==='market';
 $('order-price').placeholder=t==='stop'?'留空 = 触发后市价':'';
 $('order-kind-label').textContent=(t==='market'?'市价':t==='limit'?'限价':'条件单')+' · USDT 结算';
 if(t==='limit'&&!$('order-price').value&&Number.isFinite(quoteLast))$('order-price').value=priceText(quoteLast,cur.sym);
 renderOrderEstimate();
}
document.querySelectorAll('[data-otype]').forEach(b=>b.addEventListener('click',()=>setOrderType(b.dataset.otype)));
function renderOrders(s){
 const rows=s.orders||[];$('order-count').textContent=rows.length;
 if(!tableVisible('ordersT','lower','orders')||!changed('orders',[rows,[...pricePrecisions]]))return;
 $('ordersT').querySelector('tbody').innerHTML=rows.map(o=>{
  const kind=(o.kind==='limit'?'限价':o.limit>0?'条件限价':'条件市价')+(o.spot?' · 现货':' · 永续'),side=o.spot?(o.side==='buy'?'买入':'卖出'):(o.side==='long'?'买入开多':'卖出开空');
  const px=o.kind==='stop'?`${priceText(o.price,o.symbol)} → ${o.limit>0?priceText(o.limit,o.symbol):'市价'}`:priceText(o.price,o.symbol);
  return `<tr><td>${escapeHtml(new Date(o.created).toLocaleString())}</td><td><b>${escapeHtml(pairName(o.symbol))}</b></td><td>${kind}</td>`+
   `<td class="${['long','buy'].includes(o.side)?'green':'red'}">${side}</td><td class="ctr">${o.symbol==='NSTAR'?units(o.lots*.01,2)+' NSTAR':o.lots}</td><td>${px}</td>`+
   `<td>${o.tp>0?priceText(o.tp,o.symbol):'—'} / ${o.sl>0?priceText(o.sl,o.symbol):'—'}</td>`+
   `<td><button type="button" class="close-position" onclick="cancelOrder(${Number(o.id)})">撤单</button></td></tr>`}).join('')||
  '<tr><td colspan="8" class="empty">暂无委托，可在下单区选择「限价」或「条件单」</td></tr>';
}
async function cancelOrder(id){
 if(orderBusy||!id)return;orderBusy=true;syncOrderControls();
 try{const j=await api('cancel',{id});if(j.ok){msg('委托已撤销');await settle(j)}}
 catch(e){msg('撤单失败：'+e)}finally{orderBusy=false;syncOrderControls()}
}
async function closePos(id){
 if(orderBusy||!id)return;
 await submitClose({id});
}
async function loanAction(path,amountId,done){
 const input=$(amountId);
 if(!input.reportValidity())return;
 const j=await api(path,{amount:Number(input.value)});
 if(j.ok){msg(done);await settle(j)}
}
function borrowMoney(){return loanAction('borrow','borrow-amount','借款已到账')}
function repayMoney(){return loanAction('repay','repay-amount','还款已完成')}
async function resetAccount(){
 if(!confirm('确定重置账号吗？余额回到 10000，杠杆回到 10，持仓和贷款全部清空，交易记录保留。'))return;
 const j=await api('reset',{});
 if(j.ok){msg('账号已重置');await settle(j)}
}
function renderDebt(s){
 const limit=s.debt_limit??100000;
 let text=`负债 ${money(s.debt??0)} / 上限 ${money(limit)}`;
 if(s.debt_since>0){
  const left=Math.max(0,30*60*1000-(Date.now()-s.debt_since));
  text+=`，超限倒计时 ${Math.floor(left/60000)}:${String(Math.floor(left/1000)%60).padStart(2,'0')}，到期自动重置`;
 }
 $('debt-status').textContent=text;
}
async function closeAll(){
 if(orderBusy||!curPos.length)return;
 if(!confirm(`确认平掉全部 ${curPos.length} 笔持仓？\n将按各币对当前报价结算，无法撤销。`))return;
 await submitClose({});
}
async function submitClose(body){
 orderBusy=true;syncOrderControls();
 try{const j=await api('close',body);if(j.ok){msg('平仓已完成');await settle(j)}}
 catch(e){msg('平仓失败：'+e)}
 finally{orderBusy=false;syncOrderControls()}
}
/* 杠杆滑块: 对数刻度的常用档位，数字框仍可输入任意整数。 */
const LEV_STOPS=[1,2,3,4,5,6,7,8,9,10];
function syncLevSlider(){
 const v=Number($('lev').value),sl=$('lev-slider');if(!(v>=1))return;
 let best=0;LEV_STOPS.forEach((x,i)=>{if(Math.abs(Math.log(x/v))<Math.abs(Math.log(LEV_STOPS[best]/v)))best=i});
 setSlider(sl,best/(LEV_STOPS.length-1)*100,!$('lev').disabled);sl.value=best;
}
$('lev-slider').addEventListener('input',e=>{$('lev').value=LEV_STOPS[Number(e.target.value)];levDirty=true;syncLevSlider()});
async function setLev(){
 if(levSaving)return;
 const input=$('lev'),value=Number(input.value);
 if(curPos.length){msg('先平仓再修改杠杆');input.value=accountLeverage;levDirty=false;return}
 if(!Number.isSafeInteger(value)||value<1||value>10){msg('杠杆须为 1–10 的整数');return}
 levSaving=true;input.disabled=$('lev-apply').disabled=true;
 $('lev-apply').textContent='保存中…';
 try{
  const j=await api('leverage',{leverage:value});
  if(j.ok){accountLeverage=value;levDirty=false;input.value=value;await settle(j)}
  else{input.value=accountLeverage;levDirty=false}
 }catch(e){msg('杠杆保存失败，请重试：'+e)}
 finally{levSaving=false;input.disabled=$('lev-apply').disabled=curPos.length>0;$('lev-apply').textContent='应用杠杆';syncLevSlider()}
}

const SEC={'1s':1,'1m':60,'5m':300,'15m':900,'30m':1800,'1h':3600,'4h':14400,'1d':86400,'1w':604800};
let kLoaded=false,lastBarLive=false;
let kSeq=0,simBars=[],simStamp=0,kFlight=null,needsK=false;
const BAR_LIMIT=500,BAR_SLACK=64,pendingSim=new Map();
let pendingSimStamp=0;
/* The chart library labels every time as UTC. Bars are handed to it shifted by +8h so its
   axis and crosshair read Beijing time, where the server opens day and week candles. */
let CHART_SHIFT=8*3600;
const toBar=k=>({time:k[0]/1000+CHART_SHIFT,open:k[1],high:k[2],low:k[3],close:k[4]});
const sameBar=(a,b)=>a&&a.time===b.time&&a.open===b.open&&a.high===b.high&&a.low===b.low&&a.close===b.close;
const preserveWicks=(old,b)=>old?{...b,open:old.open,high:Math.max(old.high,b.high),low:Math.min(old.low,b.low)}:b;
function mergeSimBars(data,stamp){
 const bars=new Map(simBars.map(b=>[b.time,b]));let changed=false;
 for(const b of data){
  const old=bars.get(b.time),next=old&&stamp<simStamp?old:preserveWicks(old,b);
  if(!sameBar(old,next)){bars.set(b.time,next);changed=true}
 }
 if(changed)simBars=[...bars.values()].sort((a,b)=>a.time-b.time).slice(-BAR_LIMIT);
 simStamp=Math.max(simStamp,stamp);lastBar=simBars.at(-1);lastBarLive=false;return changed;
}
const isSim=sym=>sym==='FXSIM'||sym==='NSTAR';
function updateSim(rows,stamp=0){
 if(!rows||!series||!kLoaded||!isSim(cur.sym)||lastSym!==cur.sym||lastItv!==cur.itv||stamp<simStamp)return;
 if(!visible('chartpane')){queueSim(rows,stamp);return}
 const data=rows.map(toBar);let reset=false;
 for(const b of data){
  const end=simBars.at(-1);
  if(!end||b.time>end.time){
   simBars.push(b);if(!reset)series.update(b);
   if(end&&b.time-end.time>SEC[cur.itv])needsK=true;
  }else if(b.time===end.time){
   const next=preserveWicks(end,b);
   if(!sameBar(end,next)){simBars[simBars.length-1]=next;if(!reset)series.update(next)}
  }else{
   // Only corrections to a closed bar / hole filling use the full-history path.
   let lo=0,hi=simBars.length;
   while(lo<hi){const mid=(lo+hi)>>>1;if(simBars[mid].time<b.time)lo=mid+1;else hi=mid}
   const i=lo,old=simBars[i];
   if(i===0&&b.time<old.time&&simBars.length>=BAR_LIMIT)continue;
   const next=preserveWicks(old?.time===b.time?old:null,b);
   if(!sameBar(old,next)){
    if(old?.time===b.time)simBars[i]=next;else simBars.splice(i,0,next);
    reset=true;
   }
  }
 }
 if(simBars.length>BAR_LIMIT+BAR_SLACK){simBars=simBars.slice(-BAR_LIMIT);reset=true}
 if(reset)series.setData(simBars);
 simStamp=stamp;lastBar=simBars.at(-1);lastBarLive=false;
 if(needsK){needsK=false;loadK()}
}
function queueSim(rows,stamp=0){
 if(!rows||stamp<Math.max(simStamp,pendingSimStamp))return;
 pendingSimStamp=stamp;
 for(const k of rows){const b=preserveWicks(pendingSim.get(k[0]),toBar(k));pendingSim.set(k[0],b)}
 while(pendingSim.size>BAR_LIMIT+BAR_SLACK)pendingSim.delete(pendingSim.keys().next().value);
 scheduleVisual('candles',()=>{
  if(!visible('chartpane')||!kLoaded)return false;
  const data=[...pendingSim.values()].sort((a,b)=>a.time-b.time).map(b=>[(b.time-CHART_SHIFT)*1000,b.open,b.high,b.low,b.close]);
  const stamp=pendingSimStamp;pendingSim.clear();pendingSimStamp=0;updateSim(data,stamp);
 });
}
function loadK(){
 const sym=cur.sym,itv=cur.itv,key=sym+'/'+itv;
 if(kFlight?.key===key)return kFlight.promise;
 if(document.hidden){needsK=true;return Promise.resolve()}
 kFlight?.controller.abort();
 const my=++kSeq,controller=new AbortController();
 const current=()=>my===kSeq&&sym===cur.sym&&itv===cur.itv;
 if(!kLoaded||sym!==lastSym||itv!==lastItv)panelStatus('chartpane','loading','Loading...');
 const promise=(async()=>{
  try{
   const [j]=await Promise.all([
    fetch(`/api/klines?symbol=${sym}&interval=${itv}&limit=500`,{signal:AbortSignal.any([controller.signal,AbortSignal.timeout(20000)])}).then(r=>r.json()),
    chart?Promise.resolve():loadChartLibrary()
   ]);
   if(!current())return;if(!j.ok)throw j.err;
   const paint=()=>{if(current())applyK(j,sym,itv)};
   if(document.hidden)scheduleVisual('history-k',paint);else paint();
  }catch(e){
   if(!current()||controller.signal.aborted)return;
   if(!kLoaded)panelStatus('chartpane','error','K线加载失败，请重试');msg('K线: '+e);
  }finally{if(kFlight?.controller===controller)kFlight=null}
 })();
 kFlight={key,controller,promise};return promise;
}
function applyK(j,sym,itv){
 initChart();if(!series)throw '图表组件未加载';
 const data=j.klines.map(toBar),reset=!kLoaded||sym!==lastSym||itv!==lastItv,latest=data.at(-1);
 if(!latest)throw '无K线数据';
 observePrices(sym,j.klines.flatMap(k=>k.slice(1,5)));series.applyOptions({priceFormat:priceFormat(sym)});
 if(isSim(sym)){
  if(reset){simBars=[];simStamp=0;mergeSimBars(data,j.updated_at||0);series.setData(simBars)}
  else if((j.updated_at||0)>=simStamp)updateSim(j.klines,j.updated_at||0);
  else if(mergeSimBars(data,j.updated_at||0))series.setData(simBars); // Stale snapshot may only fill missing history.
 }else{
  // Preserve authoritative OHLC and observed live extremes, never rewind a newer bar.
  if(!reset&&lastBar&&lastBar.time>=latest.time){
   if(lastBar.time===latest.time){
    lastBar={...latest,open:lastBar.open,high:Math.max(latest.high,lastBar.high),low:Math.min(latest.low,lastBar.low),close:lastBarLive?lastBar.close:latest.close};
    data[data.length-1]=lastBar;
   }
  }else{lastBar={...latest};lastBarLive=false}
  if(reset)series.setData(data);else data.slice(-2).forEach(b=>{try{series.update(b)}catch(_){}});
 }
 if(reset){
  chart.priceScale('right').applyOptions({autoScale:true});
  const n=data.length;chart.timeScale().setVisibleLogicalRange({from:n-60,to:n+3});
  lastSym=sym;lastItv=itv;kLoaded=true;
 }
 drawPosLines();panelStatus('chartpane');requestRender();
}
function updateLast(price){
 if(isSim(cur.sym)||!series||!kLoaded||lastSym!==cur.sym||lastItv!==cur.itv||!price)return;
 // Legacy upstream history is bucketed in UTC (weeks from Monday); match it, then shift.
 const offset=cur.itv==='1w'?3*86400:0;
 const t=Math.floor((Date.now()/1000+offset)/SEC[cur.itv])*SEC[cur.itv]-offset+CHART_SHIFT;
 const base=(lastBar&&lastBar.time===t)?lastBar:{time:t,open:price,high:price,low:price,close:price};
 lastBar={time:t,open:base.open,high:Math.max(base.high,price),low:Math.min(base.low,price),close:price};
 lastBarLive=true;series.update(lastBar);
}
function render(s){
 if(!Array.isArray(s.positions))throw Error('账户数据不可用');
 lastState=s;curPos=s.positions;accountLeverage=s.leverage;observeTrade(s.last_trade);
 if(s.leaderboard_me!==undefined){lbMe=s.leaderboard_me;scheduleVisual('lb',paintLeaderboard)}
 if(s.symbol===cur.sym&&s.price!==undefined)acceptQuote(s.price,s.symbol,s.bid,s.ask,s.quote_valid,s.quote_at);
 scheduleVisual('state',()=>paintState(lastState));
}
function paintState(s){
 const ml=s.margin_level,stopout=s.stopout_level??.5;
 const mls=ml==null?'-':Math.round(ml*100)+'%',mlc=ml==null?'':(ml>=1?'green':(ml>=stopout?'':'red'));
 htmlOnce($('acct'),`<div class="account-stats">`+
  `<div class="account-stat"><span>账户余额 / USD</span><b>${money(s.balance)}</b></div>`+
  `<div class="account-stat"><span>账户净值 / USD</span><b class="${s.equity>=s.balance?'green':'red'}">${money(s.equity)}</b></div>`+
  `<div class="account-stat"><span>已用保证金 / USD</span><b>${money(s.used)}</b></div>`+
  `<div class="account-stat"><span>可用保证金 / USD</span><b>${money(s.equity-s.used)}</b></div></div>`+
  `<div class="account-meta"><span>保证金比例 <strong class="${mlc}">${mls}</strong></span>`+
  `<span>强平线 <strong>${Math.round(stopout*100)}%</strong></span><span>杠杆 <strong>1:${s.leverage}</strong></span></div>`);
 renderDebt(s);
 accountLeverage=s.leverage;
 if(!levSaving&&(!levDirty||s.positions.length)){$('lev').value=s.leverage;levDirty=false}
 $('lev').disabled=$('lev-apply').disabled=levSaving||s.positions.length>0;
 $('lev-hint').textContent=s.positions.length?'先平仓再修改杠杆':`当前 ${s.leverage} 倍 · 高杠杆更易强平`;
 syncLevSlider();renderTierHint(s);
 observePrices(s.symbol,[s.price]);
 for(const p of s.positions)observePrices(p.sym||s.symbol,[p.now]);
 curPos=s.positions;drawPosLines();$('position-count').textContent=curPos.length;
 if(tableVisible('pos','lower','pos')&&changed('positions',[s.positions,s.leverage,[...pricePrecisions]]))$('pos').querySelector('tbody').innerHTML=s.positions.map(p=>{
  const sym=p.sym||s.symbol,long=p.side=='long',roe=Number.isFinite(p.roe)?` (${(p.roe*100).toFixed(2)}%)`:'';
  return `<tr><td class="pos-sym"><b>${escapeHtml(pairName(sym))}</b><span class="lev-badge">${p.leverage??s.leverage}x</span><small>#${p.id} 永续</small></td>`+
  `<td><span class="side-badge ${long?'long':'short'}">${long?'多':'空'}</span></td>`+
  `<td class="ctr">${p.lots}${sym==='NSTAR'&&p.units!=null?`<br><small class="muted">${units(p.units)} NSTAR</small>`:''}</td>`+
  `<td>${priceText(p.entry,sym)}</td><td>${priceText(p.now,sym)}</td>`+
  `<td class="${p.liq?'red':'muted'}">${p.liq?priceText(p.liq,sym):'—'}</td>`+
  `<td class="${p.pnl>=0?'green':'red'}">${money(p.pnl)}${roe}</td><td>${money(p.margin)}</td>`+
  `<td><span class="tpsl">${p.tp?priceText(p.tp,sym):'—'} / ${p.sl?priceText(p.sl,sym):'—'}</span> <button type="button" aria-label="设置 #${p.id} 止盈止损" onclick="openTpsl(${p.id})">设置</button></td>`+
  `<td><button class="close-position" aria-label="平仓 ${escapeHtml(sym)} 持仓 #${p.id}" onclick="closePos(${p.id})">平仓</button></td></tr>`;
 }).join('')||'<tr><td colspan="10" class="empty">暂无持仓，去下单吧</td></tr>';
 renderAssets(s);renderOrders(s);renderOrderEstimate();renderSpotForm();
 syncOrderControls();panelStatus('positions');
}
let seenTrade=0,historySeq=0;
let tradeCursor=false;
function observeTrade(trade){
 // The first state only aligns the cursor (boot already fetched history), even when the
 // account has no trades yet; every later new trade reloads history.
 const first=!tradeCursor;tradeCursor=true;
 if(!trade||trade.id<=seenTrade)return;
 seenTrade=trade.id;
 if(trade.kind==='stopout'&&trade.currency!=='LEGACY')msg(`已触发强平 #${trade.position_id} ${trade.symbol}：保证金比例 ${(trade.margin_level*100).toFixed(1)}%，已实现盈亏 ${money(trade.pnl)}。详情见成交记录。`);
 if(!(first&&(historyLoaded||flights.has('history'))))loadHistory();
}
function escapeHtml(value){
 return String(value).replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
}
let historyLoaded=false,historyDirty=false;
function loadHistory(){
 if(document.hidden){historyDirty=true;return Promise.resolve()}
 if(flights.has('history')){historyDirty=true;return flights.get('history')}
 return singleflight('history',fetchHistory).finally(()=>{if(historyDirty&&!document.hidden)loadHistory()});
}
async function fetchHistory(){
 historyDirty=false;
 const my=++historySeq;panelStatus('history','loading','Loading...');
 try{
  const r=await fetch('/api/history',{headers:authH(),signal:AbortSignal.timeout(20000)});
  if(r.status===401){logout();return}
  const j=await r.json();if(!j.ok)throw j.err||'成交记录不可用';if(my!==historySeq)return;
  scheduleVisual('history',()=>{
  $('historyT').querySelector('tbody').innerHTML=j.rows.map(t=>
   `<tr><td>${escapeHtml(new Date(t.time).toLocaleString())}</td><td class="${t.kind==='stopout'?'red':''}">${({open:'开仓',close:'平仓',stopout:'强平',tp:'止盈',sl:'止损',cancel:'撤单',void:'委托失效',spot:t.side==='buy'?'现货买入':'现货卖出',funding:'资金费',reset:'重置',borrow:'借款',repay:'还款'})[t.kind]??t.kind}${t.currency==='LEGACY'?' · 旧口径':''}</td>`+
   `<td>${t.position_id||'—'}</td><td>${escapeHtml(pairName(t.symbol))}</td><td>${({long:'多',short:'空',buy:'买',sell:'卖'})[t.side]??'—'}</td>`+
   `<td>${t.kind==='spot'?units(t.lots*.01)+' NSTAR':t.lots}</td><td>${t.entry>0?priceText(t.entry,t.symbol):'—'}</td><td>${priceText(t.price,t.symbol)}</td><td class="${t.pnl==null?'':t.pnl>=0?'green':'red'}">${t.pnl==null?'-':t.currency==='LEGACY'&&!/USD(T)?$/.test(t.symbol)&&t.symbol!=='FXSIM'?'旧口径 '+amountFormat.format(t.pnl):money(t.pnl)}</td><td>${money(t.balance)}</td></tr>`).join('')||'<tr><td colspan="10" class="empty">暂无成交记录</td></tr>';
  historyLoaded=true;panelStatus('history');
  });
 }catch(e){if(my===historySeq)panelStatus('history','error','成交记录加载失败，请重试')}
}
let connWasDown=false;
function connDown(){connWasDown=true;scheduleVisual('connection',()=>{$('connection').dataset.state='down';$('connection').textContent='重连中';$('connection').title='与服务器的连接暂时中断，正在重试'})}
function connOk(){connWasDown=false;scheduleVisual('connection',()=>{$('connection').dataset.state='ready';$('connection').textContent='已连接';$('connection').title='服务器连接正常'})}
function refresh(){return singleflight('state',fetchState)}
async function fetchState(){
 if(!$('positions-status').hidden)panelStatus('positions','loading','Loading...');
 try{const r=await fetch('/api/state',{headers:authH(),signal:AbortSignal.timeout(20000)});
  if(r.status===401){logout();return}
  const s=await r.json();if(!s.ok)throw s.err||'行情不可用';
  connOk();
  render(s);
 }catch(e){
  if(!$('positions-status').hidden)panelStatus('positions','error','持仓加载失败，请重试');
  connDown();
 }
}
/* ws: all packets count as liveness, including the quiet 15s server heartbeat. */
let ws=null,wsTimer=null,wsLive=false,wsAttempts=0,wsEverOpened=false,wsLastMessage=0,wsOpenedAt=0;
const pollNeeded=()=>!wsLive&&!document.hidden;
function sendView(){
 if(ws?.readyState!==1)return;
 ws.send(JSON.stringify({type:'view',visible:!document.hidden,leaderboard:paneVisible('upper','rank'),interval:cur.itv,low_power:lowPower}));
}
function reconnectLater(){
 if(wsTimer!==null)return;
 const delay=Math.min(30000,1000*2**Math.min(wsAttempts++,5))*(.75+Math.random()*.5);
 wsTimer=setTimeout(()=>{wsTimer=null;wsConnect()},delay);
}
function checkSocket(){
 if(document.hidden||!wsLive)return;
 if(Date.now()-wsOpenedAt>30000)wsAttempts=0;
 if(Date.now()-wsLastMessage>60000)ws.close();
}
function wsConnect(){
 if(ws&&(ws.readyState===0||ws.readyState===1))return;
 let socket;
 try{socket=new WebSocket(`${location.protocol==='https:'?'wss':'ws'}://${location.host}/ws?token=${encodeURIComponent(TOKEN)}`);ws=socket}catch(e){connDown();reconnectLater();return}
 socket.onmessage=e=>{
  if(socket!==ws)return;wsLastMessage=Date.now();
  let d;try{d=JSON.parse(e.data)}catch(_){return}
  if(d.type==='auth_expired'||d.type==='unauthorized'){logout();return}
  if(d.type==='resync'){
   if(d.history)loadHistory();
   if(d.chat){if(document.hidden)chatDirty=true;else loadChat()}
   return;
  }
  if(d.type==='symbols'){applySymbols(d);return}
  if(d.type==='chat'){renderChat([d.row]);return}
  if(d.type==='lb'){renderLb(d.rows,d);return}
  if(d.type==='nstar'){nstarEvent(d);return}
  if(d.sym==='NSTAR'&&d.market)renderMarket(d.market);
  if(d.type==='state'){connOk();render(d);return}
  if(d.sym!==cur.sym)return;
  acceptQuote(d.price,d.sym,d.bid,d.ask,d.quote_valid,d.quote_at??d.time);
  if(isSim(d.sym))queueSim(d.klines&&d.klines[cur.itv],d.time);
 };
 socket.onopen=()=>{
  if(socket!==ws)return;const recovering=wsEverOpened;wsEverOpened=true;wsLive=true;wsLastMessage=wsOpenedAt=Date.now();connOk();sendView();
  if(recovering){needsK=true;chatDirty=true;if(!document.hidden){needsK=false;loadK();loadChat();loadSymbols()}}
 };
 socket.onclose=e=>{
  if(socket!==ws)return;wsLive=false;quoteReady=false;connDown();syncOrderControls();
  if(e?.code===4001||e?.code===4401){logout();return}
  // A removed/expired account must be checked even if visuals are paused.
  refresh();reconnectLater();
 };
 socket.onerror=()=>socket.close();
}
/* Top 50 push + independent personal rank; full standings are requested one page at a time. */
let lbTop=[],lbRows=[],lbMe=null,lbTotal=0,lbAsOf=0,lbOffset=0,lbLoaded=false,lbSeq=0;
function renderLb(rows,meta={}){
 const stamp=meta.as_of??0;if(stamp&&stamp<lbAsOf)return;
 lbAsOf=Math.max(lbAsOf,stamp);lbTop=(rows||[]).slice(0,50);
 if(lbOffset===0)lbRows=lbTop;
 lbTotal=meta.total??rows.length;
 if(meta.me!==undefined)lbMe=meta.me;
 else if(!lbMe){const mine=rows.find(p=>p.name===ME);if(mine)lbMe={...mine,rank:mine.rank??rows.indexOf(mine)+1}}
 lbLoaded=true;scheduleVisual('lb',paintLeaderboard);
}
function paintLeaderboard(){
 if(!paneVisible('upper','rank'))return false;
 htmlOnce($('rankT').querySelector('tbody'),lbRows.map((p,i)=>
  `<tr class="${p.name==ME?'me':''}"><td>${p.rank??lbOffset+i+1}</td><td class="${p.online?'online':'offline'}" title="${p.online?'在线':'离线'}">${escapeHtml(p.name)}</td>`+
  `<td>${money(p.equity)}</td><td class="${p.ret>=0?'green':'red'}">${amountFormat.format(p.ret*100)}%</td>`+
  `<td class="${p.pnl_5m==null?'':p.pnl_5m>=0?'green':'red'}">${p.pnl_5m==null?'—':money(p.pnl_5m)+(p.ret_5m==null?'':`<br>${amountFormat.format(p.ret_5m*100)}%`)}</td></tr>`).join('')||'<tr><td colspan="5" class="empty">暂无排名</td></tr>');
 $('rank-me').textContent=lbMe?.rank?`我的排名 #${lbMe.rank} · ${money(lbMe.equity)}`:'我的排名：等待账户更新';
 $('rank-page').textContent=`${lbOffset+1}–${lbOffset+lbRows.length} / ${lbTotal}`;
 $('rank-prev').disabled=lbOffset===0;$('rank-next').disabled=lbOffset+50>=lbTotal;
 panelStatus('lbbar');
}
function leaderboardPage(step){lbOffset=Math.max(0,lbOffset+step*50);loadLeaderboard()}
function loadLeaderboard(){
 if(!paneVisible('upper','rank'))return Promise.resolve();
 const offset=lbOffset;
 return singleflight('lb-'+offset,async()=>{
  const my=++lbSeq,stamp=lbAsOf;
  if(!lbLoaded)panelStatus('lbbar','loading','Loading...');
  try{
   const r=await fetch(`/api/leaderboard?offset=${offset}&limit=50`,{headers:authH(),signal:AbortSignal.timeout(20000)});
   if(r.status===401){logout();return}
   const j=await r.json();if(!j.ok)throw j.err||'排行不可用';
   if(my!==lbSeq||offset!==lbOffset)return;
   if(offset===0){if(!j.as_of&&lbAsOf>stamp)return;renderLb(j.rows,j)}
   else{lbRows=j.rows.slice(0,50);lbTotal=j.total??lbTotal;if(j.me!==undefined)lbMe=j.me;lbLoaded=true;scheduleVisual('lb',paintLeaderboard)}
  }catch(e){if(my===lbSeq&&!$('lbbar-status').hidden)panelStatus('lbbar','error','排行加载失败，请重试')}
 });
}

/* chat: 复用WS广播，首屏/重连读取历史，无定时轮询。 */
let chatRows=[],chatSending=false,chatDirty=false;
const chatNodes=new Map();
function renderChat(rows){
 const known=new Set(chatRows.map(row=>row.id));let added=false;
 for(const row of rows||[])if(row&&!known.has(row.id)){chatRows.push(row);known.add(row.id);added=true}
 if(added)chatRows.sort((a,b)=>a.id-b.id);
 chatRows=chatRows.slice(-100);scheduleVisual('chat',paintChat);
}
function paintChat(){
 if(!paneVisible('chat','chat'))return false;
 const list=$('chat-messages'),stick=list.scrollHeight-list.scrollTop-list.clientHeight<50;
 const keep=new Set(chatRows.map(row=>row.id));let changed=false;
 for(const [id,node] of chatNodes)if(!keep.has(id)){node.remove();chatNodes.delete(id);changed=true}
 if(chatRows.length){
  if(!chatNodes.size)list.replaceChildren();
  for(let i=0;i<chatRows.length;i++){
   const row=chatRows[i];let node=chatNodes.get(row.id);
   if(!node){
    node=document.createElement('div');node.className='chat-item';node.dataset.id=row.id;
    node.innerHTML=`<b>${escapeHtml(row.name)}</b><time>${escapeHtml(new Date(row.time).toLocaleTimeString())}</time><div>${escapeHtml(row.text)}</div>`;
    chatNodes.set(row.id,node);list.insertBefore(node,list.children[i]||null);changed=true;
   }
  }
 }else htmlIfChanged(list,'<div class="empty">暂无消息，聊聊吧</div>');
 if(changed&&stick)list.scrollTop=list.scrollHeight;panelStatus('chatpane');
}
function loadChat(){return singleflight('chat',async()=>{
 chatDirty=false;
 if(paneVisible('chat','chat')&&!$('chatpane-status').hidden)panelStatus('chatpane','loading','Loading...');
 try{const r=await fetch('/api/chat',{headers:authH(),signal:AbortSignal.timeout(20000)});
  if(r.status===401){logout();return}
  const j=await r.json();if(!j.ok)throw j.err||'聊天不可用';renderChat(j.rows);
 }catch(e){if(paneVisible('chat','chat')&&!$('chatpane-status').hidden)panelStatus('chatpane','error','聊天加载失败，请重试')}
})}
async function sendChat(){
 if(chatSending)return;
 const text=$('chat-text').value.trim();if(!text)return;
 chatSending=true;$('chat-send').disabled=true;$('chat-error').textContent='';
 try{
  const r=await fetch('/api/chat',{method:'POST',headers:authH({'Content-Type':'application/json'}),body:JSON.stringify({text}),signal:AbortSignal.timeout(20000)});
  const j=await r.json();if(!j.ok)throw j.err||'发送失败';renderChat([j.row]);
  if($('chat-text').value.trim()===text)$('chat-text').value='';
 }catch(e){$('chat-error').textContent=String(e)}
 finally{chatSending=false;$('chat-send').disabled=false}
}

/* ---------- exchange panels: mode tabs, spot, TP/SL, market data ---------- */
const units=(v,d=4)=>Number.isFinite(v)?v.toLocaleString('en-US',{maximumFractionDigits:d}):'—';
const compact=v=>!Number.isFinite(v)?'—':v>=1e8?(v/1e8).toFixed(2)+'亿':v>=1e4?(v/1e4).toFixed(2)+'万':units(v,2);
function setMode(next){
 mode=next;
 for(const m of ['perp','spot'])$('mode-'+m).setAttribute('aria-selected',m===mode);
 $('perp-form').hidden=$('lev-form').hidden=$('close-box').hidden=mode!=='perp';
 $('spot-form').hidden=mode!=='spot';
 setSpotType(spotType);if(lastState)scheduleVisual('state',()=>paintState(lastState));sendView();
}
function setSpotSide(side){
 spotSide=side;
 for(const s of ['buy','sell'])$('spot-side-'+s).setAttribute('aria-selected',s===side);
 const b=$('spot-submit');b.className='spot-submit '+side;b.textContent=(side==='buy'?'买入':'卖出')+' NSTAR';
 renderSpotForm();
}
function setSpotType(t){
 spotType=t;document.querySelectorAll('[data-spot-otype]').forEach(b=>b.setAttribute('aria-selected',b.dataset.spotOtype===t));
 $('spot-trigger-field').hidden=t!=='stop';$('spot-price-field').hidden=t==='market';
 $('spot-price').placeholder=t==='stop'?'留空 = 触发后市价':'';
 if(t==='limit'&&!$('spot-price').value&&Number.isFinite(nstarMid()))$('spot-price').value=priceText(nstarMid(),'NSTAR');
 renderSpotForm();
}
document.querySelectorAll('[data-spot-otype]').forEach(b=>b.addEventListener('click',()=>setSpotType(b.dataset.spotOtype)));
function nstarMid(){return cur.sym==='NSTAR'&&Number.isFinite(quoteLast)?quoteLast:lastState?.nstar_price??NaN}
function heldNstar(){return (lastState?.spot||[]).find(h=>h.sym==='NSTAR')?.units??0}
function renderSpotForm(){
 if(!visible('spot-form'))return;
 const px=nstarMid(),n=Number($('spot-units').value),price=Number($('spot-price').value)||0,trigger=Number($('spot-trigger').value)||0;
 const reference=spotType==='market'?NaN:spotType==='limit'?price:(price||trigger);
 const avg=n>0?(Number.isFinite(reference)?reference:bookFill(spotSide==='buy'?n:-n)):NaN,est=Number.isFinite(avg)?avg:px;
 $('spot-total').textContent=Number.isFinite(est)&&n>0?`≈ ${units(est*n*(1+(spotSide==='buy'?1:-1)*(lastState?.spot_fee??.001)),2)} USDT`:'—';
 $('spot-avail').textContent=lastState?spotSide==='buy'?`${units(lastState.balance,2)} USDT · 最多 ${units(spotMax(),2)} NSTAR`:`${units(heldNstar())} NSTAR`:'—';
 const max=spotMax();setSlider($('spot-slider'),max>0&&n>0?n/max*100:0,max>0);
 const f=n>0&&spotType==='market'?bookFill(spotSide==='buy'?n:-n):NaN,imp=f/px-1,node=$('spot-fill');
 node.textContent=spotType!=='market'?(spotType==='limit'?'按委托价或更优成交':'触发后按市价 / 委托价'):Number.isFinite(f)?`${units(f,4)}（${imp>=0?'+':''}${(imp*100).toFixed(2)}%）`:'—';
 node.className=Number.isFinite(imp)&&Math.abs(imp)>IMPACT_WARN?'red':'';
}
function spotMax(){
 if(!lastState)return 0;
 const exact=lastState.limits?.spot_max_buy,px=nstarMid(),sign=spotSide==='buy'?1:-1;
 const raw=spotSide==='sell'?heldNstar():Number.isFinite(exact)?exact:Number.isFinite(px)&&px>0?lastState.balance/(px*1.003):0;
 return raw;
}
function spotPercent(f){
 if(!lastState)return;
 $('spot-units').value=Math.max(0,Math.floor(spotMax()*f*1e4)/1e4);renderSpotForm();
}
$('spot-slider').addEventListener('input',e=>spotPercent(Number(e.target.value)/100));
/* 档位杠杆: 每一段仓位按该段杠杆计保证金，超过最高档按 1x；总持仓不限。 */
function renderTierHint(s){
 const t=s?.limits?.tiers||[],node=$('tier-hint');node.hidden=!t.length||s.symbol!==cur.sym;if(node.hidden)return;
 const gross=s.limits.gross_units||0,found=t.findIndex(([upTo])=>gross<upTo),at=found<0?t.length-1:found,last=t.at(-1)[0];
 node.textContent='档位杠杆：'+t.map(([upTo,lev],i)=>`${i===at&&gross>0?'▶':''}≤${compact(upTo)} ${lev}x`).join(' · ')+
  ` ｜ 超过 ${compact(last)} 1x · 当前 ${units(gross,2)} 枚 · 总持仓不限`;
}
async function spotTrade(){
 if(orderBusy||!$('spot-units').reportValidity())return;
 if(!tradableSymbols?.includes(cur.sym)){msg('该币对暂不可新开仓');return}
 if(!usableQuote()){quoteReady=false;syncOrderControls();msg('报价不可用，请等待更新');return}
 const units=Number($('spot-units').value),price=Number($('spot-price').value)||0,trigger=Number($('spot-trigger').value)||0;
 if(spotType==='limit'&&!(price>0)){msg('请填写现货委托价');return}
 if(spotType==='stop'&&!(trigger>0)){msg('请填写现货触发价');return}
 orderBusy=true;syncOrderControls();
 try{
  const j=spotType==='market'?await api('spot',{side:spotSide,units})
   :await api('order',{market:'spot',kind:spotType,side:spotSide,units,price:spotType==='limit'?price:trigger,limit:spotType==='stop'?price:0});
  if(j.ok){msg(spotType==='market'?`NSTAR 现货${spotSide==='buy'?'买入':'卖出'}已成交`:`NSTAR 现货${spotType==='limit'?'限价':'条件'}委托已提交，可在「委托」中查看`);await settle(j)}
 }catch(e){msg('现货下单失败：'+e)}
 finally{orderBusy=false;syncOrderControls();renderSpotForm()}
}
$('spot-units').addEventListener('input',renderSpotForm);
$('spot-price').addEventListener('input',renderSpotForm);
$('spot-trigger').addEventListener('input',renderSpotForm);
function renderOrderEstimate(){
 if(!visible('perp-form'))return;
 const s=lastState,lots=Number($('lots').value);
 if(!s||s.symbol!==cur.sym||!(lots>0)){setText($('order-notional'),'—');setText($('order-margin'),'—');return}
 const notional=lots*(s.contract??1)*s.price,tiers=s.limits?.tiers||[];
 let margin=notional/(s.symbol_leverage||s.leverage);
 if(tiers.length){
  // 与后端 tiered_margin_units 一致：只有新增的那一段按所在档位计保证金。
  const tm=g=>{let from=0,m=0;tiers.forEach(([upTo,lev],i)=>{const to=i===tiers.length-1?g:Math.min(g,upTo);if(to>from)m+=(to-from)/lev;from=upTo});return m},
   held=s.limits.gross_units||0;
  margin=(tm(held+lots*(s.contract??1))-tm(held))*s.price;
 }
 setText($('order-notional'),`${units(notional,2)} USDT`);
 setText($('order-margin'),`${units(margin,2)} USDT · ${units(margin>0?notional/margin:s.symbol_leverage||s.leverage,1)}x`);
 setText($('lots-unit'),cur.sym==='NSTAR'?`手 (=${units(lots*(s.contract??1))} NSTAR)`:STOCKS.includes(cur.sym)?'股':'手');
 syncLotPresets();
}
$('lots').addEventListener('input',renderOrderEstimate);
function renderAssets(s){
 const rows=s.spot||[];$('spot-count').textContent=rows.length;
 if(!tableVisible('spotT','lower','spot')||!changed('assets',rows))return;
 $('spotT').querySelector('tbody').innerHTML=rows.map(h=>
  `<tr><td><b>${escapeHtml(h.sym)}</b></td><td>${units(h.units)}</td><td>${units(h.cost,4)}</td><td>${units(h.price,4)}</td>`+
  `<td>${units(h.value,2)}</td><td class="${h.pnl>=0?'green':'red'}">${money(h.pnl)}</td></tr>`).join('')||
  '<tr><td colspan="6" class="empty">暂无现货资产，切到「现货」买入 NSTAR</td></tr>';
}
function openTpsl(id){
 const p=curPos.find(p=>p.id===id);if(!p)return;tpslId=id;
 $('tpsl-info').textContent=`#${p.id} ${pairName(p.sym)} ${p.side==='long'?'多':'空'} ${p.lots} · 开仓 ${priceText(p.entry,p.sym)}`;
 $('tpsl-tp').value=p.tp?priceText(p.tp,p.sym):'';$('tpsl-sl').value=p.sl?priceText(p.sl,p.sym):'';
 const d=$('tpsl-dialog');if(d.showModal)d.showModal();
}
function closeTpsl(){const d=$('tpsl-dialog');if(d.close)d.close()}
async function saveTpsl(){
 const j=await api('tpsl',{id:tpslId,tp:Number($('tpsl-tp').value)||0,sl:Number($('tpsl-sl').value)||0});
 if(j.ok){closeTpsl();msg('止盈止损已更新');await settle(j)}
}
/* 盘口: NSTAR 的弹性 AMM 曲线可精确推算每一档；其他币对按点差画示意盘口。 */
const bookNodes=new Map();
function renderBook(mid){
 if(!paneVisible('upper','book')){scheduleVisual('book',()=>{if(!paneVisible('upper','book'))return false;renderBook(latestQuote?.price??mid)});return}
 if(!Number.isFinite(mid)){$('book-last').textContent='—';return}
 if(!changed('book',[mid,cur.sym,market?.depth,market?.amm_width,market?.anchor,market?.fair,pricePrecisions.get(cur.sym)]))return;
 const star=cur.sym==='NSTAR',depth=market?.depth||50000,dp=pricePrecisions.get(cur.sym)??decimalPlaces(mid);
 // 锚价偏离 x 处流动性 = depth·(1+x²/w²)，库存 U(x)=depth·(x+x³/3w²)，与后端 amm_shift 一致。
 const w=market?.amm_width||0,x0=star&&w&&market?.anchor>0?Math.log(mid/market.anchor):0;
 const inv=x=>depth*(x+(w?x**3/(3*w*w):0)),cumAt=p=>Math.abs(inv(x0+Math.log(p/mid))-inv(x0));
 const step=star?mid*.0005:Math.max(10**-dp,mid*.0001),rows=11,asks=[],bids=[];
 let sumA=0,sumB=0;
 for(let i=1;i<=rows;i++){
  const up=mid+step*i,dn=mid-step*i;
  sumA+=1+(i*7)%5;sumB+=1+(i*3)%5;
  asks.push([up,star?cumAt(up):sumA]);bids.push([dn,star?cumAt(dn):sumB]);
 }
 const max=Math.max(asks.at(-1)[1],bids.at(-1)[1]);
 for(const [side,values] of [['ask',asks],['bid',bids]]){
  const container=$('book-'+side+'s');let nodes=bookNodes.get(side);
  if(!nodes){
   nodes=Array.from({length:11},()=>{
    const node=document.createElement('div');node.className='book-row '+side;
    for(let i=0;i<3;i++)node.append(document.createElement('span'));
    node.onclick=()=>pickPrice(Number(node.dataset.price));container.append(node);return node;
   });bookNodes.set(side,nodes);
  }
  values.forEach(([p,cum],i)=>{
   const node=nodes[side==='ask'?10-i:i],prev=i?values[i-1][1]:0;
   const depth=(cum/max*100).toFixed(1)+'%';node.dataset.price=p;
   if(node.dataset.depth!==depth){node.dataset.depth=depth;node.style.setProperty('--depth',depth)}
   [priceText(p,cur.sym),units(cum-prev,star?2:0),units(cum,star?2:0)].forEach((text,j)=>{if(node.children[j].textContent!==text)node.children[j].textContent=text});
  });
 }
 const node=$('book-last');setText(node,priceText(mid,cur.sym));setClass(node,$('price').dataset.direction==='down'?'red':'green');
 setText($('book-spread'),star&&market?`公允 ${units(market.fair,2)}`:'');
}
function pickPrice(p){if(mode==='spot')renderSpotForm();msg(`档位价格 ${priceText(p,cur.sym)}`)}
function renderTape(rows,seq){
 if(!paneVisible('chat','tape')){scheduleVisual('tape',()=>{if(!paneVisible('chat','tape'))return false;renderTape(market?.tape??rows,market?.tape_seq??seq)});return}
 if(!changed('tape',seq===undefined?rows:seq))return;
 $('tape-rows').innerHTML=(rows||[]).map(([t,p,u])=>`<div class="tape-row"><span class="${u>=0?'t-buy':'t-sell'}">${priceText(p,'NSTAR')}</span>`+
  `<span>${units(Math.abs(u),2)}</span><span>${escapeHtml(new Date(t).toLocaleTimeString())}</span></div>`).join('')||
  '<div class="empty">NSTAR 暂无成交，来做第一笔吧</div>';
}
function renderMarket(m){
 market=m;
 if(m&&Object.prototype.hasOwnProperty.call(m,'prev_close')){
  quoteReferences.set('NSTAR',{close:Number.isFinite(m.prev_close)&&m.prev_close>0?m.prev_close:undefined,at:Date.now()});
  scheduleVisual('quote-change',renderQuoteChange);
 }
 scheduleVisual('market',()=>paintMarket(market));
}
let nstarStats=null;
function paintMarket(m){
 const star=cur.sym==='NSTAR';
 if(nstarStats!==star){nstarStats=star;document.querySelectorAll('[data-nstar]').forEach(el=>el.hidden=!star)}
 if(!m)return;
 if(star){
  setText($('stat-mark'),units(m.mark??quoteLast,4));setText($('stat-fair'),units(m.fair,4));
  setText($('stat-high'),units(m.high_24h,4));setText($('stat-low'),units(m.low_24h,4));
  setText($('stat-vol'),compact(m.volume_24h));setText($('stat-turnover'),compact(m.turnover_24h));
  const f=$('stat-funding');setText(f,`${(m.funding_rate*100).toFixed(4)}%`);setClass(f,m.funding_rate>=0?'green':'red');
 }
 renderTape(m.tape,m.tape_seq);renderBook(latestQuote?.price??quoteLast);
 if(paneVisible('upper','book'))panelStatus('lbbar');
 if(paneVisible('chat','tape'))panelStatus('chatpane');
}
function fundingCountdown(){
 if(document.hidden)return;
 const left=3600000-(Date.now()%3600000),node=$('stat-countdown');
 if(node)setText(node,new Date(left).toISOString().slice(11,19));
}
async function loadMarket(){
 try{const r=await fetch('/api/market',{signal:AbortSignal.timeout(20000)}),j=await r.json();if(j.ok)renderMarket(j)}catch(_){}
}
function bindTabs(attr){
 document.querySelectorAll(`[data-${attr}]`).forEach(btn=>btn.onclick=()=>{
  const key=btn.dataset[attr];
  document.querySelectorAll(`[data-${attr}]`).forEach(b=>b.setAttribute('aria-selected',b===btn));
  document.querySelectorAll(`[data-${attr}-pane]`).forEach(p=>p.hidden=p.dataset[attr+'Pane']!==key);
  sendView();if(lastState)scheduleVisual('state',()=>paintState(lastState));
  if(attr==='upper'&&key==='rank'){if(lbLoaded)scheduleVisual('lb',paintLeaderboard);else loadLeaderboard()}
  if(attr==='upper'&&key==='book'){panelStatus('lbbar');renderBook(latestQuote?.price??quoteLast)}
  if(attr==='chat'){panelStatus('chatpane');if(key==='chat')scheduleVisual('chat',paintChat);else renderTape(market?.tape,market?.tape_seq)}
  requestRender();
 });
}
['lower','upper','chat'].forEach(bindTabs);
/* boot: HTTP seeds once; the first WS open only sends the subscription. */
let booted=false;
$('low-power').checked=lowPower;
const syncLowPowerClass=()=>document.documentElement?.classList?.toggle('low-power',lowPower);
syncLowPowerClass();
$('low-power').onchange=()=>{
 lowPower=$('low-power').checked;try{localStorage.fx_low_power=String(lowPower)}catch(_){}
 syncLowPowerClass();
 clearTimeout(frameTimer);frameTimer=null;sendView();requestRender();
};
function visibilityChanged(){
 sendView();
 if(document.hidden){
  if(frame!==null)cancelAnimationFrame(frame);frame=null;clearTimeout(frameTimer);frameTimer=null;
  needsK=true;return;
 }
 // Reset the silence clock after OS/browser suspension; a 500ms quote is not a heartbeat deadline.
 wsLastMessage=Date.now();lastFrame=-Infinity;
 refresh();needsK=false;loadK();if(historyDirty)loadHistory();if(chatDirty)loadChat();
 if(paneVisible('upper','rank'))loadLeaderboard();requestRender();
}
async function boot(){
 if(booted)return;
 booted=true;
 if(!await me()){booted=false;$('auth').style.display='flex';$('shell').hidden=true;return}
 $('auth').style.display='none';$('shell').hidden=false;
 resizeSides();new ResizeObserver(resizeSides).observe($('wrap'));
 loadSymbols();refresh();loadK();loadHistory();loadChat();loadMarket();wsConnect();
 if(paneVisible('upper','rank'))loadLeaderboard();
 setInterval(()=>{if(pollNeeded())refresh()},5000);
 setInterval(()=>{if(pollNeeded())loadK()},30000);
 setInterval(()=>{if(!document.hidden)scheduleVisual('countdown',fundingCountdown)},1000);
 setInterval(checkSocket,10000);
 document.addEventListener('visibilitychange',visibilityChanged);
}
if(TOKEN)boot();else{$('auth').style.display='flex'}

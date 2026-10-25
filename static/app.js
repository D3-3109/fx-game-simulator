
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
let TOKEN=localStorage.fx_token||'',ME='',cur={sym:'USDJPY',itv:'15m'};
let chart=null,series=null,lastBar=null,priceLines=[],curPos=[],symSynced=false,lastSym=null,lastItv=null;
let levDirty=false,levSaving=false,accountLeverage=100,orderBusy=false,authBusy=false;
let quoteSymbol='',quoteLast=null,quoteReady=false,quoteTimer=null;
let mode='perp',spotSide='buy',lastState=null,market=null,tpslId=0,lotsFor='';
const quoteReferences=new Map();
function renderQuote(price,symbol,bid,ask,valid=true,at){
 if(symbol!==cur.sym)return;
 const fx=symbol!=='FXSIM'&&!symbol.endsWith('USDT');
 observePrices(symbol,fx?[price,bid,ask]:[price]);
 const sideText=value=>fx||symbol in FIXED_PRECISION?priceText(value,symbol):value.toFixed(Math.max(pricePrecisions.get(symbol)??0,4));
 clearTimeout(quoteTimer);
 quoteReady=valid&&Number.isFinite(bid)&&Number.isFinite(ask)&&bid>0&&ask>=bid&&(!Number.isFinite(at)||Date.now()-at<=30000);
 $('buy-price').textContent=quoteReady?sideText(ask):'—';
 $('sell-price').textContent=quoteReady?sideText(bid):'—';
 if(quoteReady&&Number.isFinite(at))quoteTimer=setTimeout(()=>{
  quoteReady=false;$('buy-price').textContent=$('sell-price').textContent='—';syncOrderControls();
 },Math.max(0,at+30000-Date.now()));
 syncOrderControls();
 const node=$('price');
 if(quoteSymbol!==symbol){quoteSymbol=symbol;quoteLast=null;delete node.dataset.direction}
 if(Number.isFinite(price)&&quoteLast!==null&&price!==quoteLast)node.dataset.direction=price>quoteLast?'up':'down';
 const formatted=priceText(price,symbol);
 document.title=`${formatted} | ${symbol==='NSTAR'?pairName(symbol):symbol}`;
 const [whole,decimal]=formatted.split('.');
 node.innerHTML=Number.isFinite(price)?Number(whole).toLocaleString('en-US')+(decimal?`<span class="quote-decimal">.${decimal}</span>`:''):'—';
 $('quote-unit').textContent=symbol==='NSTAR'||symbol.endsWith('USDT')?'USDT':isSim(symbol)||STOCKS.includes(symbol)?'USD':symbol.slice(-3);
 quoteLast=Number.isFinite(price)?price:null;
 renderQuoteChange();
 if(quoteLast!==null)loadQuoteReference(symbol);
}
function renderQuoteChange(){
 const reference=quoteReferences.get(quoteSymbol)?.close,node=$('quote-change');
 node.textContent='';node.className='quote-change';
 if(quoteSymbol!==cur.sym||quoteLast===null||!Number.isFinite(reference)||reference<=0)return;
 const delta=Number((quoteLast-reference).toFixed(pricePrecisions.get(quoteSymbol)??decimalPlaces(quoteLast))),sign=delta>0?'+':'';
 node.textContent=`${sign}${priceText(delta,quoteSymbol)}  ${sign}${(delta/reference*100).toFixed(2)}%`;
 node.className+=' '+(delta===0?'muted':delta>0?'green':'red');
}
async function loadQuoteReference(symbol){
 const previous=quoteReferences.get(symbol);
 if(previous&&(previous.pending||Date.now()-previous.at<60000))return;
 const reference={at:Date.now(),pending:true};quoteReferences.set(symbol,reference);
 try{
  const r=await fetch(`/api/klines?symbol=${symbol}&interval=1d&limit=2`,{signal:AbortSignal.timeout(20000)}),j=await r.json();
  if(!j.ok||!Array.isArray(j.klines)||j.klines.length<2)return;
  const close=j.klines.at(-2)[4];if(Number.isFinite(close)&&close>0)reference.close=close;
 }catch(_){}  // 没有前日收盘数据时留空，不用当前价或开盘价冒充。
 finally{reference.pending=false;if(symbol===cur.sym)renderQuoteChange()}
}
function panelStatus(id,state='ready',text=''){
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

/* 版本与SRI固定；CDN失败/5秒超时用同版本本地文件，旧charts.js保留兼容旧页面。 */
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
  try{await load('https://unpkg.com/lightweight-charts@5.2.1/dist/lightweight-charts.standalone.production.js')}
  catch(_){await load('/assets/lightweight-charts-5.2.1.js')}
 })().catch(error=>{chartLibraryPromise=null;throw error});
 return chartLibraryPromise;
}

/* chart / trading 同前 */
const UP='#25a750',DOWN='#ca3f64';
function initChart(){
 if(chart||typeof LightweightCharts==='undefined')return;
 const cw=$('cv').clientWidth||innerWidth-24;
 chart=LightweightCharts.createChart($('cv'),{width:cw,height:$('cv').clientHeight||160,
  layout:{background:{type:'solid',color:'#000000'},textColor:'#8a8a8a',fontSize:11,fontFamily:'Noto Sans, PingFang SC, Noto Sans SC, sans-serif'},
  grid:{vertLines:{color:'#161616'},horzLines:{color:'#161616'}},crosshair:{mode:0},
  timeScale:{borderColor:'#232323',timeVisible:true,secondsVisible:false},
  rightPriceScale:{borderColor:'#232323'}});
 chart.subscribeCrosshairMove(param=>{
  const p=param.seriesData&&param.seriesData.get(series);
  $('px').textContent=p?`开 ${priceText(p.open)} 高 ${priceText(p.high)} 低 ${priceText(p.low)} 收 ${priceText(p.close)}`:'';});
 series=chart.addSeries(LightweightCharts.CandlestickSeries,{upColor:UP,downColor:DOWN,borderVisible:false,
  wickUpColor:UP,wickDownColor:DOWN,
  priceFormat:priceFormat(cur.sym)});
 new ResizeObserver(()=>{
  const cv=$('cv');if(cv.clientWidth&&cv.clientHeight)chart.applyOptions({width:cv.clientWidth,height:cv.clientHeight});
 }).observe($('cv'));
}
function drawPosLines(){
 if(!series)return;priceLines.forEach(l=>series.removePriceLine(l));priceLines=[];
 for(const p of curPos.filter(p=>p.sym===cur.sym))priceLines.push(series.createPriceLine({
  price:p.entry,color:p.side=='long'?UP:DOWN,lineWidth:1,lineStyle:2,
  axisLabelVisible:true,title:(p.side=='long'?'多 ':'空 ')+p.lots}));
}
const STOCKS=['AAPL','TSLA','NVDA','MSFT','AMZN','GOOGL','META','AMD'];
const symSel=$('sym'),itvSel=$('itv');
const symbolLabel=s=>s==='NSTAR'?'NSTAR/USDT · 北极星币':s==='FXSIM'?'FXSIM · 模拟币':pairName(s);
'NSTAR EURUSDT BTCUSDT ETHUSDT SOLUSDT XRPUSDT BNBUSDT DOGEUSDT ADAUSDT TRXUSDT AVAXUSDT LINKUSDT EURUSD GBPUSD USDJPY USDCHF USDCAD AUDUSD NZDUSD EURJPY GBPJPY USDCNY USDCNH FXSIM'.split(' ').forEach(s=>symSel.add(new Option(symbolLabel(s),s)));
function syncSecondOption(){
 const fx=cur.sym==='FXSIM'||cur.sym==='NSTAR';
 if(!fx&&cur.itv==='1s')cur.itv='1m';
 itvSel.replaceChildren();
 (fx?['1s','1m','5m','15m','30m','1h','4h','1d','1w']:['1m','5m','15m','30m','1h','4h','1d','1w']).forEach(i=>itvSel.add(new Option(i)));
 itvSel.value=cur.itv;
 $('itv-quick').innerHTML=(fx?['1s','1m','5m','15m','1h','4h','1d']:['1m','5m','15m','1h','4h','1d']).map(i=>
  `<button type="button" class="itv" aria-pressed="${i===cur.itv}" onclick="pickItv('${i}')">${i}</button>`).join('');
}
function pickItv(i){itvSel.value=i;itvSel.onchange()}
syncSecondOption();
symSel.value=cur.sym;itvSel.value=cur.itv;
let tradableSymbols=null,symbolsVersion=-1,symbolSaving=false;
function syncSymbolChoices(){
 if(tradableSymbols!==null){
  symSel.replaceChildren();tradableSymbols.forEach(s=>symSel.add(new Option(symbolLabel(s),s)));
  if(!tradableSymbols.includes(cur.sym)){
   const option=new Option(cur.sym+' · 已禁用（仅查看）',cur.sym);option.disabled=true;symSel.add(option);
  }
  symSel.value=cur.sym;
 }
 syncSecondOption();
 syncOrderControls();
}
function syncOrderControls(){
 const allowed=tradableSymbols!==null&&tradableSymbols.includes(cur.sym);
 symSel.disabled=orderBusy||symbolSaving||tradableSymbols?.length===0;
 $('order-symbol').textContent=pairName(cur.sym)+(cur.sym==='NSTAR'?' 永续':'');
 const whole=STOCKS.includes(cur.sym),star=cur.sym==='NSTAR';
 $('lots').min=whole?1:0.01;$('lots').step=whole?1:0.01;
 // NSTAR: 1 手 = 0.01 枚 ≈ $1；预设对应 $100 / $1k / $10k / $100k 名义价值。
 document.querySelectorAll('[data-lots]').forEach((b,i)=>{const v=(whole?[1,10,50,100]:star?[100,1000,10000,100000]:[0.01,0.1,0.5,1])[i];b.dataset.lots=v;b.textContent=whole?String(v):star?(v>=1000?v/1000+'k':String(v)):v.toFixed(2)});
 if(lotsFor!==cur.sym){lotsFor=cur.sym;$('lots').value=star?'1000':whole?'1':'0.10'}
 syncLotPresets();renderOrderEstimate();
 $('trade-buy').disabled=$('trade-sell').disabled=orderBusy||symbolSaving||!allowed||!quoteReady;
 $('close-all').disabled=orderBusy||curPos.length===0;
 $('close-all').textContent=orderBusy?'正在处理…':'全部平仓';
 document.querySelectorAll('.close-position').forEach(button=>button.disabled=orderBusy);
 $('symbol-status').textContent=orderBusy?'订单提交中，请稍候…':tradableSymbols===null?'正在获取币种配置…':allowed?'':
  tradableSymbols.length?'该币对已禁用，已有持仓仍可平仓':'暂无启用的币对，已有持仓仍可平仓';
 if(allowed&&!orderBusy&&cur.sym==='NSTAR')$('symbol-status').textContent='NSTAR 杠杆上限 1:100 · 偏离锚价越远流动性越厚 · 单账户合约/现货上限各 20,000 枚';
}
function applySymbols(settings){
 if(settings.version<symbolsVersion)return;
 symbolsVersion=settings.version;tradableSymbols=settings.rows.filter(row=>row.enabled).map(row=>row.symbol);
 syncSymbolChoices();
}
async function loadSymbols(){
 try{const r=await fetch('/api/symbols',{signal:AbortSignal.timeout(20000)}),j=await r.json();
  if(!j.ok)throw j.err||'币种配置不可用';applySymbols(j);
 }catch(e){$('symbol-status').textContent='币种配置加载失败，请刷新重试'}
}
symSel.onchange=async()=>{
 const previous=cur.sym,next=symSel.value;if(symbolSaving||next===previous)return;
 symbolSaving=true;syncSymbolChoices();
 try{const j=await api('symbol',{symbol:next});
  if(j.ok){cur.sym=next;syncSecondOption();renderQuote(NaN,next);renderMarket(market);renderBook(NaN);if(series)series.applyOptions({priceFormat:priceFormat(next)});lastBar=null;kLoaded=false;drawPosLines();loadK();await settle(j)}
 }catch(e){msg('币对切换失败：'+e)}
 finally{symbolSaving=false;syncSymbolChoices()}
};
itvSel.onchange=()=>{cur.itv=itvSel.value;lastBar=null;kLoaded=false;syncSecondOption();drawPosLines();loadK()};
syncSymbolChoices();

async function api(path,body){const r=await fetch('/api/'+path,{method:'POST',headers:authH({'Content-Type':'application/json'}),body:JSON.stringify(body||{})});
 if(r.status==401){logout();throw'auth'}const j=await r.json();if(!j.ok)msg('操作失败：'+j.err);return j}
// Mutations return the new account; only fall back to a GET when the reply lacks it.
const settle=j=>j.state?(connOk(),render(j.state)):refresh();
let messageTimer=null;
function msg(t){clearTimeout(messageTimer);$('msg').textContent=t;messageTimer=setTimeout(()=>$('msg').textContent='',6000)}
function syncLotPresets(){
 document.querySelectorAll('[data-lots]').forEach(button=>button.setAttribute('aria-pressed',Number(button.dataset.lots)===Number($('lots').value)));
}
function setLots(value){const n=Number(value);$('lots').value=STOCKS.includes(cur.sym)||cur.sym==='NSTAR'&&n>=1?String(n):n.toFixed(2);syncLotPresets()}
/* NSTAR 快讯: 传闻/消息/巨鲸，所有人可见，仅保留最近 4 条。 */
let nstarFeed=[];
function nstarEvent(e){
 nstarFeed=[...nstarFeed,e].slice(-4);
 $('nstar-feed').innerHTML=nstarFeed.map(x=>`<li data-kind="${escapeHtml(x.kind)}"><time>${escapeHtml(new Date(x.time).toLocaleTimeString())}</time>${escapeHtml(x.text)}</li>`).join('');
 syncOrderControls();
}
$('lots').addEventListener('input',syncLotPresets);syncLotPresets();
async function trade(side){
 if(orderBusy)return;
 if(symbolSaving||tradableSymbols===null||!tradableSymbols.includes(cur.sym)){msg('该币对暂不可新开仓');return}
 if(!quoteReady){msg('报价不可用，请等待更新');return}
 if(!$('lots').reportValidity())return;
 orderBusy=true;syncOrderControls();
 try{const j=await api('trade',{side,lots:Number($('lots').value)});
  if(j.ok){msg(`${cur.sym} ${side==='buy'?'买入':'卖出'}已成交`);await settle(j)}
 }catch(e){msg('下单失败：'+e)}
 finally{orderBusy=false;syncOrderControls()}
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
 if(!confirm('确定重置账号吗？余额回到 10000，杠杆回到 100，持仓和贷款全部清空，交易记录保留。'))return;
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
async function setLev(){
 if(levSaving)return;
 const input=$('lev'),value=Number(input.value);
 if(curPos.length){msg('先平仓再修改杠杆');input.value=accountLeverage;levDirty=false;return}
 if(!Number.isSafeInteger(value)||value<1){msg('杠杆必须为正整数');return}
 levSaving=true;input.disabled=$('lev-apply').disabled=true;
 $('lev-apply').textContent='保存中…';
 try{
  const j=await api('leverage',{leverage:value});
  if(j.ok){accountLeverage=value;levDirty=false;input.value=value;await settle(j)}
  else{input.value=accountLeverage;levDirty=false}
 }catch(e){msg('杠杆保存失败，请重试：'+e)}
 finally{levSaving=false;input.disabled=$('lev-apply').disabled=curPos.length>0;$('lev-apply').textContent='应用杠杆'}
}

const SEC={'1s':1,'1m':60,'5m':300,'15m':900,'30m':1800,'1h':3600,'4h':14400,'1d':86400,'1w':604800};
let kLoaded=false,lastBarLive=false;
let kSeq=0,simBars=[],simStamp=0;
function mergeSimBars(data,stamp){
 if(stamp<simStamp)return false;  // 慢轮询不能覆盖较新的实时OHLC。
 const bars=new Map(simBars.map(b=>[b.time,b]));
 data.forEach(b=>bars.set(b.time,b));
 simBars=[...bars.values()].sort((a,b)=>a.time-b.time).slice(-500);simStamp=stamp;
 lastBar={...simBars[simBars.length-1]};lastBarLive=false;return true;
}
const isSim=sym=>sym==='FXSIM'||sym==='NSTAR';
function updateSim(rows,stamp){
 if(!rows||!series||!kLoaded||!isSim(cur.sym)||lastSym!==cur.sym||lastItv!==cur.itv)return;
 const data=rows.map(k=>({time:k[0]/1000,open:k[1],high:k[2],low:k[3],close:k[4]}));
 if(mergeSimBars(data,stamp))series.setData(simBars);
}
async function loadK(){
 const my=++kSeq,sym=cur.sym,itv=cur.itv;  // 请求守卫: 慢响应过期即弃
 if(!kLoaded||sym!==lastSym||itv!==lastItv)panelStatus('chartpane','loading','Loading...');
 try{const r=await fetch(`/api/klines?symbol=${sym}&interval=${itv}&limit=500`,{signal:AbortSignal.timeout(20000)});
  const j=await r.json();if(!j.ok)throw j.err;
  if(my!==kSeq||sym!==cur.sym||itv!==cur.itv)return;  // 已切走, 丢包
  if(!chart)await loadChartLibrary();
  if(my!==kSeq||sym!==cur.sym||itv!==cur.itv)return;
  initChart();if(!series)throw '图表组件未加载';
  const data=j.klines.map(k=>({time:k[0]/1000,open:k[1],high:k[2],low:k[3],close:k[4]}));
  const reset=!kLoaded||cur.sym!==lastSym||cur.itv!==lastItv,latest=data[data.length-1];
  if(!latest)throw '无K线数据';
  observePrices(sym,j.klines.flatMap(k=>k.slice(1,5)));
  series.applyOptions({priceFormat:priceFormat(sym)});
  if(isSim(sym)){
   if(reset){simBars=[];simStamp=0}
   if(!mergeSimBars(data,j.updated_at||0))return;
   series.setData(simBars);  // 后端完整OHLC含闭合前一根，不从抽样价格猜影线。
  }else{
   // 保留当前蜡烛完整OHLC；慢轮询不能抹掉已观察的影线或回退最新tick。
   if(!reset&&lastBar&&lastBar.time>=latest.time){
    if(lastBar.time===latest.time){
     lastBar={...latest,open:lastBar.open,high:Math.max(latest.high,lastBar.high),
      low:Math.min(latest.low,lastBar.low),close:lastBarLive?lastBar.close:latest.close};
     data[data.length-1]=lastBar;
    }
   }else{lastBar={...latest};lastBarLive=false}
   if(reset)series.setData(data);
   else data.slice(-2).forEach(b=>{try{series.update(b)}catch(_){}});
  }
  if(reset){
   chart.priceScale('right').applyOptions({autoScale:true});
   const n=data.length;chart.timeScale().setVisibleLogicalRange({from:n-60,to:n+3});
   lastSym=cur.sym;lastItv=cur.itv;kLoaded=true;
  }
  drawPosLines();panelStatus('chartpane');
 }catch(e){
  if(my!==kSeq||sym!==cur.sym||itv!==cur.itv)return;
  if(!kLoaded)panelStatus('chartpane','error','K线加载失败，请重试');
  msg('K线: '+e);
 }
}
function updateLast(price){
 if(isSim(cur.sym)||!series||!kLoaded||lastSym!==cur.sym||lastItv!==cur.itv||!price)return;
 const offset=cur.itv==='1w'?3*86400:0;
 const t=Math.floor((Date.now()/1000+offset)/SEC[cur.itv])*SEC[cur.itv]-offset;
 const base=(lastBar&&lastBar.time===t)?lastBar:{time:t,open:price,high:price,low:price,close:price};
 lastBar={time:t,open:base.open,high:Math.max(base.high,price),low:Math.min(base.low,price),close:price};
 lastBarLive=true;series.update(lastBar);
}
function render(s){
 const ml=s.margin_level,stopout=s.stopout_level??.5;
 const mls=ml==null?'-':Math.round(ml*100)+'%',mlc=ml==null?'':(ml>=1?'green':(ml>=stopout?'':'red'));
 $('acct').innerHTML=`<div class="account-stats">`+
  `<div class="account-stat"><span>账户余额 / USD</span><b>${money(s.balance)}</b></div>`+
  `<div class="account-stat"><span>账户净值 / USD</span><b class="${s.equity>=s.balance?'green':'red'}">${money(s.equity)}</b></div>`+
  `<div class="account-stat"><span>已用保证金 / USD</span><b>${money(s.used)}</b></div>`+
  `<div class="account-stat"><span>可用保证金 / USD</span><b>${money(s.equity-s.used)}</b></div></div>`+
  `<div class="account-meta"><span>保证金比例 <strong class="${mlc}">${mls}</strong></span>`+
  `<span>强平线 <strong>${Math.round(stopout*100)}%</strong></span><span>杠杆 <strong>1:${s.leverage}</strong></span></div>`;
 renderDebt(s);
 accountLeverage=s.leverage;
 if(!levSaving&&(!levDirty||s.positions.length)){$('lev').value=s.leverage;levDirty=false}
 $('lev').disabled=$('lev-apply').disabled=levSaving||s.positions.length>0;
 $('lev-hint').textContent=s.positions.length?'先平仓再修改杠杆':`当前 ${s.leverage} 倍 · 高杠杆更易强平`;
 observePrices(s.symbol,[s.price]);
 for(const p of s.positions)observePrices(p.sym||s.symbol,[p.now]);
 curPos=s.positions;drawPosLines();$('position-count').textContent=curPos.length;
 $('pos').querySelector('tbody').innerHTML=s.positions.map(p=>{
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
 lastState=s;renderAssets(s);renderOrderEstimate();renderSpotForm();
 syncOrderControls();panelStatus('positions');observeTrade(s.last_trade);
 if(s.symbol===cur.sym){renderQuote(s.price,s.symbol,s.bid,s.ask,s.quote_valid,s.quote_at);updateLast(s.price)}
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
 if(!(first&&historyLoaded))loadHistory();
}
function escapeHtml(value){
 return String(value).replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
}
let historyLoaded=false;
async function loadHistory(){
 const my=++historySeq;panelStatus('history','loading','Loading...');
 try{
  const r=await fetch('/api/history',{headers:authH(),signal:AbortSignal.timeout(20000)});
  const j=await r.json();if(!j.ok)throw j.err||'成交记录不可用';if(my!==historySeq)return;
  $('historyT').querySelector('tbody').innerHTML=j.rows.map(t=>
   `<tr><td>${escapeHtml(new Date(t.time).toLocaleString())}</td><td class="${t.kind==='stopout'?'red':''}">${({open:'开仓',close:'平仓',stopout:'强平',tp:'止盈',sl:'止损',spot:t.side==='buy'?'现货买入':'现货卖出',funding:'资金费',reset:'重置',borrow:'借款',repay:'还款'})[t.kind]??t.kind}${t.currency==='LEGACY'?' · 旧口径':''}</td>`+
   `<td>${t.position_id||'—'}</td><td>${escapeHtml(pairName(t.symbol))}</td><td>${({long:'多',short:'空',buy:'买',sell:'卖'})[t.side]??'—'}</td>`+
   `<td>${t.kind==='spot'?units(t.lots*.01)+' NSTAR':t.lots}</td><td>${t.entry>0?priceText(t.entry,t.symbol):'—'}</td><td>${priceText(t.price,t.symbol)}</td><td class="${t.pnl==null?'':t.pnl>=0?'green':'red'}">${t.pnl==null?'-':t.currency==='LEGACY'&&!/USD(T)?$/.test(t.symbol)&&t.symbol!=='FXSIM'?'旧口径 '+amountFormat.format(t.pnl):money(t.pnl)}</td><td>${money(t.balance)}</td></tr>`).join('')||'<tr><td colspan="10" class="empty">暂无成交记录</td></tr>';
  historyLoaded=true;panelStatus('history');
 }catch(e){if(my===historySeq)panelStatus('history','error','成交记录加载失败，请重试')}
}
let connWasDown=false;
function connDown(){connWasDown=true;$('connection').dataset.state='down';$('connection').textContent='重连中';$('connection').title='与服务器的连接暂时中断，正在重试'}
function connOk(){connWasDown=false;$('connection').dataset.state='ready';$('connection').textContent='已连接';$('connection').title='服务器连接正常'}
async function refresh(){
 if(!$('positions-status').hidden)panelStatus('positions','loading','Loading...');
 try{const r=await fetch('/api/state',{headers:authH(),signal:AbortSignal.timeout(20000)});
  if(r.status===401){logout();return}
  const s=await r.json();if(!s.ok)throw s.err||'行情不可用';
  connOk();
  if(!symSynced&&s.symbol){symSynced=true;cur.sym=s.symbol;syncSymbolChoices();lastSym=null;loadK()}
  render(s);
 }catch(e){
  if(!$('positions-status').hidden)panelStatus('positions','error','持仓加载失败，请重试');
  connDown();
 }
}
/* ws */
let ws=null,wsTimer=null,wsLive=false;
const pollNeeded=()=>!wsLive&&!document.hidden;
function wsConnect(){
 try{ws=new WebSocket(`${location.protocol==='https:'?'wss':'ws'}://${location.host}/ws?token=${TOKEN}`)}catch(e){return}
 ws.onmessage=e=>{const d=JSON.parse(e.data);
  if(d.type==='symbols'){applySymbols(d);return}
  if(d.type==='chat'){renderChat([d.row]);return}
  if(d.type==='lb'){renderLb(d.rows);return}
  if(d.type==='nstar'){nstarEvent(d);return}
  if(d.sym==='NSTAR'&&d.market)renderMarket(d.market);
  if(d.type==='state'){connOk();render(d);return}
  if(d.sym!==cur.sym)return;
  renderQuote(d.price,d.sym,d.bid,d.ask,d.quote_valid,d.quote_at);renderBook(d.price);
  if(isSim(d.sym)){updateSim(d.klines&&d.klines[cur.itv],d.time);return}
  updateLast(d.price)};
 ws.onopen=()=>{wsLive=true;connOk();loadChat();loadSymbols()};
 ws.onclose=()=>{wsLive=false;connDown();wsTimer=setTimeout(wsConnect,2000)};
}
/* rank: ws推送渲染 */
function renderLb(rows){
 $('rankT').querySelector('tbody').innerHTML=rows.map((p,i)=>
  `<tr class="${p.name==ME?'me':''}"><td>${i+1}</td><td class="${p.online?'online':'offline'}" title="${p.online?'在线':'离线'}">${escapeHtml(p.name)}</td>`+
  `<td>${money(p.equity)}</td><td class="${p.ret>=0?'green':'red'}">${amountFormat.format(p.ret*100)}%</td>`+
  `<td class="${p.pnl_5m==null?'':p.pnl_5m>=0?'green':'red'}">${p.pnl_5m==null?'—':money(p.pnl_5m)+(p.ret_5m==null?'':`<br>${amountFormat.format(p.ret_5m*100)}%`)}</td></tr>`).join('')||'<tr><td colspan="5" class="empty">暂无排名</td></tr>';
 panelStatus('lbbar');
}
async function loadLeaderboard(){
 if(!$('lbbar-status').hidden)panelStatus('lbbar','loading','Loading...');
 try{const r=await fetch('/api/leaderboard',{signal:AbortSignal.timeout(20000)});
  const j=await r.json();if(!j.ok)throw j.err||'排行不可用';renderLb(j.rows);
 }catch(e){if(!$('lbbar-status').hidden)panelStatus('lbbar','error','排行加载失败，请重试')}
}

/* chat: 复用WS广播，首屏/重连读取历史，无定时轮询。 */
let chatRows=[],chatSending=false;
function renderChat(rows){
 const list=$('chat-messages'),stick=list.scrollHeight-list.scrollTop-list.clientHeight<50;
 const merged=new Map(chatRows.map(row=>[row.id,row]));rows.forEach(row=>merged.set(row.id,row));
 chatRows=[...merged.values()].sort((a,b)=>a.id-b.id).slice(-100);
 list.innerHTML=chatRows.map(row=>`<div class="chat-item"><b>${escapeHtml(row.name)}</b><time>${escapeHtml(new Date(row.time).toLocaleTimeString())}</time><div>${escapeHtml(row.text)}</div></div>`).join('')||'<div class="empty">暂无消息，聊聊吧</div>';
 if(stick)list.scrollTop=list.scrollHeight;panelStatus('chatpane');
}
async function loadChat(){
 if(!$('chatpane-status').hidden)panelStatus('chatpane','loading','Loading...');
 try{const r=await fetch('/api/chat',{headers:authH(),signal:AbortSignal.timeout(20000)});
  const j=await r.json();if(!j.ok)throw j.err||'聊天不可用';renderChat(j.rows);
 }catch(e){if(!$('chatpane-status').hidden)panelStatus('chatpane','error','聊天加载失败，请重试')}
}
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
 if(mode==='spot'&&cur.sym!=='NSTAR')switchSymbol('NSTAR');
 renderSpotForm();
}
function setSpotSide(side){
 spotSide=side;
 for(const s of ['buy','sell'])$('spot-side-'+s).setAttribute('aria-selected',s===side);
 const b=$('spot-submit');b.className='spot-submit '+side;b.textContent=(side==='buy'?'买入':'卖出')+' NSTAR';
 renderSpotForm();
}
function nstarMid(){return cur.sym==='NSTAR'&&Number.isFinite(quoteLast)?quoteLast:lastState?.nstar_price??NaN}
function heldNstar(){return (lastState?.spot||[]).find(h=>h.sym==='NSTAR')?.units??0}
function renderSpotForm(){
 const px=nstarMid(),n=Number($('spot-units').value);
 $('spot-total').textContent=Number.isFinite(px)&&n>0?`≈ ${units(px*n*(1+(spotSide==='buy'?1:-1)*(lastState?.spot_fee??.001)),2)} USDT`:'—';
 $('spot-avail').textContent=lastState?spotSide==='buy'?`${units(lastState.balance,2)} USDT`:`${units(heldNstar())} NSTAR`:'—';
}
function spotPercent(f){
 const px=nstarMid();if(!lastState)return;
 const n=spotSide==='buy'?(Number.isFinite(px)&&px>0?lastState.balance*f/(px*1.003):0):heldNstar()*f;
 $('spot-units').value=Math.max(0,Math.floor(n*1e4)/1e4);renderSpotForm();
}
async function spotTrade(){
 if(orderBusy)return;if(!$('spot-units').reportValidity())return;
 orderBusy=true;syncOrderControls();$('spot-submit').disabled=true;
 try{const j=await api('spot',{side:spotSide,units:Number($('spot-units').value)});
  if(j.ok){msg(`NSTAR 现货${spotSide==='buy'?'买入':'卖出'}已成交`);await settle(j)}
 }catch(e){msg('现货下单失败：'+e)}
 finally{orderBusy=false;$('spot-submit').disabled=false;syncOrderControls()}
}
$('spot-units').addEventListener('input',renderSpotForm);
function renderOrderEstimate(){
 const s=lastState,lots=Number($('lots').value);
 if(!s||s.symbol!==cur.sym||!(lots>0)){$('order-notional').textContent=$('order-margin').textContent='—';return}
 const notional=lots*(s.contract??1)*s.price;
 $('order-notional').textContent=`${units(notional,2)} USDT`;
 $('order-margin').textContent=`${units(notional/(s.symbol_leverage||s.leverage),2)} USDT · ${s.symbol_leverage||s.leverage}x`;
 $('lots-unit').textContent=cur.sym==='NSTAR'?`手 (=${units(lots*(s.contract??1))} NSTAR)`:STOCKS.includes(cur.sym)?'股':'手';
}
$('lots').addEventListener('input',renderOrderEstimate);
function renderAssets(s){
 const rows=s.spot||[];$('spot-count').textContent=rows.length;
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
function renderBook(mid){
 if(!Number.isFinite(mid)){$('book-asks').innerHTML=$('book-bids').innerHTML='';$('book-last').textContent='—';return}
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
 const row=(cls,[p,cum],prev)=>`<div class="book-row ${cls}" style="--depth:${(cum/max*100).toFixed(1)}%" onclick="pickPrice(${p})">`+
  `<span>${priceText(p,cur.sym)}</span><span>${units(cum-prev,star?2:0)}</span><span>${units(cum,star?2:0)}</span></div>`;
 $('book-asks').innerHTML=asks.map((a,i)=>row('ask',a,i?asks[i-1][1]:0)).reverse().join('');
 $('book-bids').innerHTML=bids.map((b,i)=>row('bid',b,i?bids[i-1][1]:0)).join('');
 const node=$('book-last');node.textContent=priceText(mid,cur.sym);node.className=$('price').dataset.direction==='down'?'red':'green';
 $('book-spread').textContent=star&&market?`公允 ${units(market.fair,2)}`:'';
}
function pickPrice(p){if(mode==='spot')renderSpotForm();msg(`档位价格 ${priceText(p,cur.sym)}`)}
function renderTape(rows){
 $('tape-rows').innerHTML=(rows||[]).map(([t,p,u])=>`<div class="tape-row"><span class="${u>=0?'t-buy':'t-sell'}">${priceText(p,'NSTAR')}</span>`+
  `<span>${units(Math.abs(u),2)}</span><span>${escapeHtml(new Date(t).toLocaleTimeString())}</span></div>`).join('')||
  '<div class="empty">NSTAR 暂无成交，来做第一笔吧</div>';
}
function renderMarket(m){
 market=m;const star=cur.sym==='NSTAR';
 document.querySelectorAll('[data-nstar]').forEach(el=>el.hidden=!star);
 if(!m)return;
 if(star){
  $('stat-mark').textContent=units(quoteLast,4);$('stat-fair').textContent=units(m.fair,4);
  $('stat-high').textContent=units(m.high_24h,4);$('stat-low').textContent=units(m.low_24h,4);
  $('stat-vol').textContent=compact(m.volume_24h);$('stat-turnover').textContent=compact(m.turnover_24h);
  const f=$('stat-funding');f.textContent=`${(m.funding_rate*100).toFixed(4)}%`;f.className=m.funding_rate>=0?'green':'red';
 }
 renderTape(m.tape);
}
function fundingCountdown(){
 const left=3600000-(Date.now()%3600000),node=$('stat-countdown');
 if(node)node.textContent=new Date(left).toISOString().slice(11,19);
}
async function loadMarket(){
 try{const r=await fetch('/api/market',{signal:AbortSignal.timeout(20000)}),j=await r.json();if(j.ok)renderMarket(j)}catch(_){}
}
function bindTabs(attr){
 document.querySelectorAll(`[data-${attr}]`).forEach(btn=>btn.onclick=()=>{
  const key=btn.dataset[attr];
  document.querySelectorAll(`[data-${attr}]`).forEach(b=>b.setAttribute('aria-selected',b===btn));
  document.querySelectorAll(`[data-${attr}-pane]`).forEach(p=>p.hidden=p.dataset[attr+'Pane']!==key);
 });
}
['lower','upper','chat'].forEach(bindTabs);
/* boot */
async function boot(){
 if(!await me()){$('auth').style.display='flex';$('shell').hidden=true;return}
 $('auth').style.display='none';$('shell').hidden=false;
 resizeSides();new ResizeObserver(resizeSides).observe($('wrap'));
 await loadSymbols();refresh();loadK();loadLeaderboard();loadHistory();loadChat();loadMarket();wsConnect();
 // WS pushes state, quotes, candles and market data; HTTP polling only covers a dropped
 // socket, and nothing polls while the tab is hidden.
 setInterval(()=>{if(pollNeeded())refresh()},5000);
 setInterval(()=>{if(pollNeeded()||!isSim(cur.sym)&&!document.hidden)loadK()},30000);
 setInterval(fundingCountdown,1000);
 setInterval(()=>{if(!document.hidden&&cur.sym!=='NSTAR'&&$('chatpane').querySelector('[data-chat-pane="tape"]')?.hidden===false)loadMarket()},15000);
 document.addEventListener('visibilitychange',()=>{if(!document.hidden){refresh();if(!wsLive)loadK()}});
}
if(TOKEN)boot();else{$('auth').style.display='flex'}

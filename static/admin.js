
const $=id=>document.getElementById(id);
const money=n=>`${n<0?'-':''}$${Math.abs(n).toFixed(2)}`;
let token=sessionStorage.fx_admin_token||'',busy=false,pendingFunding=null;
function notice(text){$('message').textContent=text}
function view(){const logged=Boolean(token);$('login').hidden=logged;$('panel').hidden=!logged;$('logout').hidden=!logged}
async function call(path,body){
 const response=await fetch('/api/admin/'+path,{method:body===undefined?'GET':'POST',
  headers:{'Content-Type':'application/json',Authorization:'Bearer '+token},body:body===undefined?undefined:JSON.stringify(body),signal:AbortSignal.timeout(20000)});
 const data=await response.json();
 if(response.status===401){token='';sessionStorage.removeItem('fx_admin_token');view()}
 if(!data.ok)throw Error(data.err||'操作失败');return data;
}
function row(values){const tr=document.createElement('tr');for(const value of values){const td=document.createElement('td');td.textContent=String(value);tr.append(td)}return tr}
async function refresh(){
 const [users,audit,symbols]=await Promise.all([call('users'),call('deposits'),call('symbols')]);
 renderSymbols(symbols.rows);
 const selected=$('target').value;$('target').replaceChildren();$('users').replaceChildren();
 for(const user of users.rows){
  $('target').add(new Option(user.name,user.name));
  const tr=row([user.name,money(user.balance),money(user.deposited),user.positions]);
  const td=document.createElement('td'),button=document.createElement('button');button.textContent='删除账户';button.className='danger';button.onclick=()=>deleteUser(user.name);td.append(button);tr.append(td);$('users').append(tr);
 }
 if(users.rows.some(u=>u.name===selected))$('target').value=selected;
 $('fund-button').disabled=!users.rows.length;
 $('audit').replaceChildren();
 for(const event of audit.rows)$('audit').append(row([new Date(event.time).toLocaleString(),
  event.kind==='deposit'?'充值':event.kind==='symbol'?(event.enabled?'启用币种':'禁用币种'):'删除账户',event.name,
  event.kind==='deposit'?money(event.amount):'-',event.balance==null?'-':money(event.balance),event.actor]));
 if(!audit.rows.length)$('audit').append(row(['暂无管理员操作记录']));
}
function renderSymbols(rows){
 $('symbols').replaceChildren();
 for(const symbol of rows){
  const tr=row([symbol.symbol,symbol.provider]),td=document.createElement('td'),checkbox=document.createElement('input');
  checkbox.type='checkbox';checkbox.checked=symbol.enabled;checkbox.setAttribute('aria-label','启用 '+symbol.symbol);
  checkbox.onchange=()=>setSymbol(checkbox,symbol.symbol,symbol.enabled);
  td.append(checkbox);tr.append(td);$('symbols').append(tr);
 }
}
async function setSymbol(checkbox,symbol,previous){
 const enabled=checkbox.checked;checkbox.disabled=true;
 try{const data=await call('symbols',{symbol,enabled});renderSymbols(data.rows);
  notice(`${symbol} 已${enabled?'启用':'禁用新开仓'}；已有仓位不受影响。`);
 }catch(error){checkbox.checked=previous;notice(error.message)}
 finally{checkbox.disabled=false}
}
$('login-form').onsubmit=async e=>{
 e.preventDefault();$('login-button').disabled=true;
 try{const data=await call('login',{psk:$('psk').value});token=data.token;sessionStorage.fx_admin_token=token;$('psk').value='';view();notice('已登录，管理会话有效1小时。');await refresh()}
 catch(error){notice(error.message)}finally{$('login-button').disabled=false}
};
$('fund-form').onsubmit=async e=>{
 e.preventDefault();if(busy)return;
 const name=$('target').value,amount=Number($('amount').value);
 if(!name||!Number.isFinite(amount)||amount<=0||amount!==Math.round(amount*100)/100){notice('请输入有效美元金额，最多两位小数。');return}
 if(!pendingFunding||pendingFunding.name!==name||pendingFunding.amount!==amount){
  const bytes=crypto.getRandomValues(new Uint8Array(16));pendingFunding={name,amount,request_id:Array.from(bytes,b=>b.toString(16).padStart(2,'0')).join('')};
 }
 busy=true;$('fund-button').disabled=true;
 try{const data=await call('deposit',pendingFunding);pendingFunding=null;notice(`${data.row.name} 充值 ${money(data.row.amount)}，余额 ${money(data.row.balance)}。`);await refresh()}
 catch(error){notice(error.message+'；网络不确定时，可点击重试原请求，不会重复充值。')}
 finally{busy=false;$('fund-button').disabled=false}
};
async function deleteUser(name){
 if(busy)return;
 const typed=prompt(`永久删除账号「${name}」？\n余额、持仓、成交记录、聊天和登录态都会删除，无法恢复。\n请输入完整账号名确认：`);
 if(typed===null)return;if(typed!==name){notice('账号名不符，未删除。');return}
 busy=true;
 try{await call('delete',{name,confirmation:typed});notice(`已永久删除账号「${name}」，管理员审计已保留。`);await refresh()}
 catch(error){notice(error.message)}finally{busy=false}
}
$('refresh').onclick=()=>refresh().catch(error=>notice(error.message));
$('logout').onclick=async()=>{try{await call('logout',{})}finally{token='';sessionStorage.removeItem('fx_admin_token');view()}};
view();if(token)refresh().catch(error=>notice(error.message));

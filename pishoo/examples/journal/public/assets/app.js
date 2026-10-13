(()=>{'use strict';
 const main=document.getElementById('journal');
 const icon=name=>`<svg aria-hidden="true"><use href="#i-${name}"/></svg>`;
 const escape=s=>String(s??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
 const displayName=name=>String(name??'').replace(/\.dhttp\.net$/i,'');
 const randomId=()=>Array.from(crypto.getRandomValues(new Uint8Array(16)),n=>n.toString(16).padStart(2,'0')).join('');
 const date=value=>new Intl.DateTimeFormat('zh-CN',{year:'numeric',month:'long',day:'numeric',weekday:'long'}).format(new Date(value));
 const time=value=>new Intl.DateTimeFormat('zh-CN',{month:'numeric',day:'numeric',hour:'2-digit',minute:'2-digit'}).format(new Date(value));
 const photoUrl=e=>`photo?id=${encodeURIComponent(e.id)}&v=${encodeURIComponent(e.version)}`;
 let context=null,view={kind:'list',entries:[],nextOffset:null};
 const drafts=new Map();
 let loadNumber=0;
 async function api(path,method='GET',doc,version){
  const headers={};if(method!=='GET'){headers['x-journal-write']='1';if(doc!==undefined)headers['content-type']='application/json';if(version)headers['if-match']=`"${version}"`;}
  const response=await fetch(path,{method,headers,body:doc===undefined?undefined:JSON.stringify(doc),cache:'no-store',credentials:'same-origin',redirect:'error'});
  if(response.status===204)return null;
  const type=response.headers.get('content-type')||'';
  const data=type.includes('application/json')?await response.json():null;
  if(response.status===202)throw new Error('此操作正在等待授权，请保留输入，获准后重试。');
  if(!response.ok){const error=new Error(data?.message||(response.status===403?'当前身份没有此操作的权限。':`请求失败（${response.status}），请保留输入后重试。`));error.status=response.status;throw error;}
  if(!data)throw new Error('未收到有效数据，请保留输入后重试。');return data;
 }
 function message(text,error=false){let target=main.querySelector('[data-message]');if(!target){target=document.createElement('div');target.dataset.message='';main.append(target);}target.className='message'+(error?' error':'');target.setAttribute('role',error?'alert':'status');target.setAttribute('aria-live','polite');target.textContent=text;}
 const scope=()=>`<span>${icon('lock')}访问范围由主页授权规则决定</span>`;
 function profile(){return `<header class="profile"><div class="avatar" aria-hidden="true">${escape(context.owner_name.slice(0,1).toUpperCase())}</div><div class="profile-copy"><h1><span class="profile-name">${escape(displayName(context.owner_name))}</span><span class="profile-caption">的手记</span></h1><p>记录生活，也留一点空白。</p><div class="scope">${icon('lock')}仅获准访问的身份可见</div></div></header>`;}
 function card(e){return `<article class="entry">${e.has_photo?`<button class="image-button" data-action="open" data-id="${escape(e.id)}" aria-label="阅读 ${escape(e.title||'手记')}"><img src="${photoUrl(e)}" alt="手记照片" loading="lazy"></button>`:''}<div class="entry-copy"><div class="meta">${date(e.created_at)}</div>${e.title?`<h3>${escape(e.title)}</h3>`:''}<p>${escape(e.preview)}</p><div class="entry-footer"><button class="button quiet" data-action="open" data-id="${escape(e.id)}">${icon('comment')}${e.comment_count?`${e.comment_count} 条留言`:'留下第一条留言'}</button><button class="button quiet read" data-action="open" data-id="${escape(e.id)}">阅读手记 ${icon('right')}</button></div></div></article>`;}
 function render(){
  let content='',months=[];
  if(view.kind==='list'){
   const groups=new Map();view.entries.forEach(e=>{const d=new Date(e.created_at),key=`${d.getFullYear()}-${d.getMonth()+1}`;if(!groups.has(key))groups.set(key,[]);groups.get(key).push(e);});
   if(!groups.size){content+=`<div class="toolbar"><h2>手记</h2>${context.is_owner?`<button class="button mobile-only" data-action="new">${icon('plus')}写手记</button>`:''}</div><div class="empty">${icon('book')}<h2>故事，从这里开始</h2><p>${context.is_owner?'写下今天的小事，或添一张喜欢的照片。':'这里还没有手记，过些时候再来看看。'}</p></div>`;}
   months=Array.from(groups.keys());
   let first=true;for(const [key,entries]of groups){const [year,month]=key.split('-');content+=`<section class="journal-month" id="month-${key}"><div class="toolbar"><h2>${month}月<span class="year">${year}</span></h2>${first&&context.is_owner?`<button class="button mobile-only" data-action="new">${icon('plus')}写手记</button>`:''}</div><div class="entry-grid">${entries.map(card).join('')}</div></section>`;first=false;}
   if(view.nextOffset!==null)content+='<div class="center"><button class="button" data-action="more">更早的手记</button></div>';
   content+='<div data-message role="status" aria-live="polite"></div><p class="footnote">慢慢写，也慢慢生活。</p>';
  }else if(view.kind==='detail'){
   const e=view.entry;content=`<div class="detail-top"><button class="button quiet" data-action="back">${icon('left')}所有手记</button>${context.is_owner?`<div class="actions"><button class="button quiet" data-action="edit">${icon('pen')}编辑</button><button class="button quiet danger" data-action="delete-entry">删除</button></div>`:''}</div><div class="reader-grid"><article class="detail">${e.has_photo?`<img src="${photoUrl(e)}" alt="手记照片">`:''}<div class="detail-copy"><div class="meta">${date(e.created_at)}</div>${e.title?`<h2>${escape(e.title)}</h2>`:''}<p class="body">${escape(e.body)}</p></div></article><section class="comments" aria-label="手记留言"><h3>留言 <span class="meta" style="display:inline">${view.comments.filter(c=>!c.deleted).length}</span></h3><div id="comment-list">${commentsHtml()}</div><form class="form" data-form="comment"><textarea class="input" name="body" aria-label="留言内容" placeholder="给这篇手记留个回应…" required maxlength="4000">${escape(drafts.get(e.id)?.body||'')}</textarea><div class="form-actions"><span>以 ${escape(displayName(context.caller_name))} 的身份留言</span><button class="button primary" type="submit">发送留言</button></div></form><div data-message role="status" aria-live="polite"></div></section></div>`;
  }else{
   const d=view.draft;content=`<div class="detail-top"><button class="button quiet" data-action="cancel-compose">${icon('left')}返回</button><span class="meta">${icon('shield')}${escape(displayName(context.caller_name))}</span></div><form class="compose" data-form="entry"><h2>${d.entry?'编辑手记':'写一篇手记'}</h2><div class="meta">${date(d.entry?.created_at||Date.now())}</div><label for="entry-title">标题（可选）</label><input id="entry-title" class="input" name="title" value="${escape(d.title)}" placeholder="给今天起个名字" maxlength="200"><label for="entry-body">正文</label><textarea id="entry-body" class="input" name="body" placeholder="今天，有什么值得记下来的？" required maxlength="32000">${escape(d.body)}</textarea><label for="entry-photo">照片</label><input id="entry-photo" type="file" accept="image/jpeg,image/png,image/webp" name="photo">${d.photoPreview?`<img class="photo-preview" src="${escape(d.photoPreview)}" alt="待发布的照片"><button type="button" class="button quiet" data-action="remove-photo">移除照片</button>`:''}<div class="scope">${icon('lock')}照片会移除定位等附带信息</div><div class="form-actions">${scope()}<button class="button primary" type="submit">${d.entry?'保存修改':'发布手记'}</button></div><div data-message role="status" aria-live="polite"></div></form>`;
  }
  main.innerHTML=`<div class="journal-layout"><aside class="journal-rail" aria-label="手记导航">${profile()}<nav class="rail-nav desktop-only" aria-label="主要导航"><button class="button all-entries" data-action="back" ${view.kind==='list'?'aria-current="page"':''}>${icon('book')}全部手记</button>${context.is_owner?`<button class="button primary" data-action="new">${icon('plus')}写手记</button>`:''}</nav>${months.length?`<nav class="rail-months desktop-only" aria-label="按月份浏览"><p>按月份浏览</p>${months.map(key=>{const [year,month]=key.split('-');return `<a href="#month-${key}">${year} 年 ${month} 月</a>`;}).join('')}</nav>`:''}<p class="rail-footnote desktop-only">慢慢写，也慢慢生活。</p></aside><div class="journal-content">${content}</div></div>`;
 }
 function commentLine(c,reply=false){return `${reply?'':`<div class="comment-avatar" aria-hidden="true">${escape((c.author||'·').slice(0,1).toUpperCase())}</div>`}<div class="comment-copy"><strong>${escape(c.deleted?'已删除的留言':displayName(c.author))}</strong>${c.deleted?'':`<span class="verified" aria-label="发布时已验证身份">${icon('shield')}</span>`}<time>${time(c.created_at)}</time><p>${escape(c.deleted?'这条留言已被主页主人删除。':c.body)}</p>${!c.deleted?`<div class="comment-actions">${!reply?`<button class="button quiet" data-action="reply" data-id="${escape(c.id)}">回复</button>`:''}${context.is_owner?`<button class="button quiet danger" data-action="delete-comment" data-id="${escape(c.id)}">删除</button>`:''}</div>`:''}</div>`;}
 function commentsHtml(){return view.comments.filter(c=>!c.parent_id).map(c=>`<div class="comment">${commentLine(c)}</div>${view.comments.some(r=>r.parent_id===c.id)?`<div class="replies">${view.comments.filter(r=>r.parent_id===c.id).map(r=>`<div class="reply">${commentLine(r,true)}</div>`).join('')}</div>`:''}<div data-reply-slot="${escape(c.id)}"></div>`).join('');}
 async function list(append=false){const number=++loadNumber;const offset=append&&view.kind==='list'?view.nextOffset:0;if(!append)main.innerHTML='<div class="busy" role="status">正在打开手记…</div>';try{const data=await api(`entries?offset=${offset||0}`);if(number!==loadNumber)return;view={kind:'list',entries:append&&view.kind==='list'?[...view.entries,...data.entries]:data.entries,nextOffset:data.next_offset};render();}catch(error){if(number!==loadNumber)return;main.innerHTML='<div class="empty"><h2>手记暂时打不开</h2><button class="button" data-action="retry">重试</button></div>';message(error.message,true);}}
 async function open(id){const number=++loadNumber;try{const [entry,comments]=await Promise.all([api(`entry?id=${encodeURIComponent(id)}`),api(`comments?entry_id=${encodeURIComponent(id)}`)]);if(number!==loadNumber)return;view={kind:'detail',entry,comments:comments.comments};render();window.scrollTo({top:0,behavior:'instant'});}catch(error){message(error.message,true);}}
 function dirty(){return view.kind==='compose'&&(view.draft.title!==view.draft.originalTitle||view.draft.body!==view.draft.originalBody||view.draft.photo!==undefined);}
 function compose(entry){++loadNumber;const d={entry,title:entry?.title||'',body:entry?.body||'',originalTitle:entry?.title||'',originalBody:entry?.body||'',photo:undefined,photoPreview:entry?.has_photo?photoUrl(entry):null,id:entry?.id||randomId()};view={kind:'compose',draft:d};render();}
 main.addEventListener('click',async event=>{
  const button=event.target.closest('[data-action]');if(!button)return;const action=button.dataset.action;
  if(action==='new'){if(dirty()&&!confirm('放弃尚未保存的修改，开始新的手记？'))return;compose();return;}
  if(action==='edit'){compose(view.entry);return;}
  if(action==='remove-photo'){view.draft.photo=null;view.draft.photoPreview=null;render();return;}
  if(action==='cancel-compose'){if(dirty()&&!confirm('这篇手记还没有保存。放弃修改并返回？'))return;const entry=view.draft.entry;if(entry)await open(entry.id);else await list();return;}
  if(action==='back'||action==='retry'){if(dirty()&&!confirm('这篇手记还没有保存。放弃修改并返回？'))return;await list();return;}
  if(action==='more'){button.disabled=true;await list(true);return;}
  if(action==='open'){button.disabled=true;await open(button.dataset.id);button.disabled=false;return;}
  if(action==='reply'){
   const parent=view.comments.find(c=>c.id===button.dataset.id);main.querySelectorAll('[data-reply-slot]').forEach(s=>{s.innerHTML='';});const slot=main.querySelector(`[data-reply-slot="${parent.id}"]`);const draft=drafts.get(parent.id);
   slot.innerHTML=`<form class="reply-form" data-form="reply" data-parent="${parent.id}"><input class="input" name="body" aria-label="回复 ${escape(displayName(parent.author))}" placeholder="回复 ${escape(displayName(parent.author))}…" value="${escape(draft?.body||'')}" required maxlength="4000"><button class="button" type="submit">发送</button></form>`;slot.querySelector('input').focus();return;
  }
  if(action==='delete-entry'||action==='delete-comment'){
   if(!confirm(action==='delete-entry'?'删除这篇手记及其全部留言？此操作无法撤销。':'删除这条留言？此操作无法撤销。'))return;
   button.disabled=true;try{if(action==='delete-entry'){await api(`entry?id=${view.entry.id}`,'DELETE',undefined,view.entry.version);await list();}else{await api(`comment?id=${button.dataset.id}`,'DELETE');await open(view.entry.id);}}catch(error){button.disabled=false;message(error.message,true);}return;
  }
 });
 main.addEventListener('input',event=>{
  if(view.kind==='compose'){if(event.target.name==='title')view.draft.title=event.target.value;if(event.target.name==='body')view.draft.body=event.target.value;}
  if(view.kind==='detail'&&event.target.name==='body'){const form=event.target.closest('form');const key=form.dataset.parent||view.entry.id;const previous=drafts.get(key);drafts.set(key,{id:previous?.id||randomId(),body:event.target.value});}
 });
 main.addEventListener('change',async event=>{
  if(event.target.name!=='photo'||view.kind!=='compose')return;const draft=view.draft,file=event.target.files[0];if(!file)return;const controls=Array.from(main.querySelectorAll('button,input,textarea'));controls.forEach(c=>c.disabled=true);
  let url;try{
   if(!['image/jpeg','image/png','image/webp'].includes(file.type)||file.size>20*1024*1024)throw new Error('请选择 20 MB 以内的 JPEG、PNG 或 WebP 照片。');
   url=URL.createObjectURL(file);const image=new Image();image.src=url;await image.decode();if(image.naturalWidth*image.naturalHeight>40_000_000)throw new Error('照片尺寸过大，请先缩小后上传。');
   const scale=Math.min(1,1600/Math.max(image.naturalWidth,image.naturalHeight));const canvas=document.createElement('canvas');canvas.width=Math.max(1,Math.round(image.naturalWidth*scale));canvas.height=Math.max(1,Math.round(image.naturalHeight*scale));const ctx=canvas.getContext('2d');ctx.fillStyle='#ffffff';ctx.fillRect(0,0,canvas.width,canvas.height);ctx.drawImage(image,0,0,canvas.width,canvas.height);
   const photo=canvas.toDataURL('image/jpeg',.82);if(photo.length>1_398_130)throw new Error('照片压缩后仍然过大，请选择更小的照片。');if(view.kind!=='compose'||view.draft!==draft)return;draft.photo=photo;draft.photoPreview=photo;render();
  }catch(error){message(error.message,true);}finally{if(url)URL.revokeObjectURL(url);controls.forEach(c=>c.disabled=false);}
 });
 main.addEventListener('submit',async event=>{
  event.preventDefault();const form=event.target,kind=form.dataset.form;if(!kind)return;const controls=Array.from(main.querySelectorAll('button,input,textarea'));controls.forEach(c=>c.disabled=true);
  try{
   if(kind==='entry'){
    const d=view.draft,doc={title:d.title.trim(),body:d.body};if(d.photo!==undefined)doc.photo=d.photo;if(!d.entry)doc.id=d.id;
    try{const saved=await api(d.entry?`entry?id=${d.id}`:'entries',d.entry?'PUT':'POST',doc,d.entry?.version);view={kind:'detail',entry:saved,comments:[]};await open(saved.id);message('已保存。');}
    catch(error){if(error.status===409&&!d.entry){const existing=await api(`entry?id=${d.id}`);if(existing.title===doc.title&&existing.body===doc.body&&!doc.photo&&!existing.has_photo){view={kind:'detail',entry:existing,comments:[]};await open(d.id);message('已确认之前的保存。');return;}}throw error;}
   }else{
    const parent=form.dataset.parent||null,key=parent||view.entry.id,body=form.querySelector('[name=body]').value.trim();if(!body)return;let draft=drafts.get(key);if(!draft){draft={id:randomId(),body};drafts.set(key,draft);}
    const id=view.entry.id;await api('comments','POST',{id:draft.id,entry_id:id,parent_id:parent,body});drafts.delete(key);await open(id);message('留言已发送。');
   }
  }catch(error){message(error.message,true);}finally{controls.forEach(c=>c.disabled=false);}
 });
 window.addEventListener('beforeunload',event=>{if(dirty()||Array.from(drafts.values()).some(d=>d.body.trim())){event.preventDefault();event.returnValue='';}});
 (async()=>{try{context=await api('context');document.getElementById('callerName').textContent=displayName(context.caller_name);document.getElementById('viewer').hidden=false;document.title=`${displayName(context.owner_name)} 的手记`;await list();}catch(error){main.innerHTML='<div class="empty"><h2>无法打开手记</h2><p>请使用已获授权的具名身份访问。</p><button class="button" data-action="reload">重试</button></div>';message(error.message,true);main.querySelector('[data-action=reload]').addEventListener('click',()=>location.reload());}})();
})();

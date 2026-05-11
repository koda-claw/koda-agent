function optHTML(text_only=false) {
function createEnhancedDOMCopy() {  
  const nodeInfo = new WeakMap();  
  const ignoreTags = ['SCRIPT', 'STYLE', 'NOSCRIPT', 'META', 'LINK', 'COLGROUP', 'COL', 'TEMPLATE', 'PARAM', 'SOURCE'];  
  const ignoreIds = ['ljq-ind'];  
  function cloneNode(sourceNode, keep=false) {  
    if (sourceNode.nodeType === 8 ||   
        (sourceNode.nodeType === 1 && (  
          ignoreTags.includes(sourceNode.tagName) ||   
          (sourceNode.id && ignoreIds.includes(sourceNode.id))  
        ))) {  
      return null;  
    }  
    if (sourceNode.nodeType === 3) return sourceNode.cloneNode(false);  
    const clone = sourceNode.cloneNode(false);
    if ((sourceNode.tagName === 'INPUT' || sourceNode.tagName === 'TEXTAREA') && sourceNode.value) clone.setAttribute('value', sourceNode.value);
    if (sourceNode.tagName === 'INPUT' && (sourceNode.type === 'radio' || sourceNode.type === 'checkbox') && sourceNode.checked) clone.setAttribute('checked', '');
    else if (sourceNode.tagName === 'SELECT' && sourceNode.value) clone.setAttribute('data-selected', sourceNode.value);  
    try { if (sourceNode.matches && sourceNode.matches(':-webkit-autofill')) { clone.setAttribute('data-autofilled', 'true'); if (!sourceNode.value) clone.setAttribute('value', '⚠️受保护-读tmwebdriver_sop的autofill章节提取'); } } catch(e) {}

    const isDropdown = sourceNode.classList?.contains('dropdown-menu') ||   
             /dropdown|menu/i.test(sourceNode.className) || sourceNode.getAttribute('role') === 'menu'; 
    const _ddItems = isDropdown ? sourceNode.querySelectorAll('a, button, [role="menuitem"], li').length : 0;
    const isSmallDropdown = _ddItems > 0 && _ddItems <= 7 && sourceNode.textContent.length < 500;  

    const childNodes = [];  
    for (const child of sourceNode.childNodes) {  
      const childClone = cloneNode(child, keep || isSmallDropdown);  
      if (childClone) childNodes.push(childClone);  
    }  
    if (sourceNode.tagName === 'IFRAME') {
      try {
        const iDoc = sourceNode.contentDocument || sourceNode.contentWindow?.document;
        if (iDoc && iDoc.body && iDoc.body.children.length > 0) {
          const wrapper = document.createElement('div');
          wrapper.setAttribute('data-iframe-content', sourceNode.src || '');
          for (const ch of iDoc.body.childNodes) {
            const c = cloneNode(ch, keep);
            if (c) wrapper.appendChild(c);
          }
          if (wrapper.childNodes.length) childNodes.push(wrapper);
        }
      } catch(e) {}
    }
    if (sourceNode.shadowRoot) {
      for (const shadowChild of sourceNode.shadowRoot.childNodes) {
        const shadowClone = cloneNode(shadowChild, keep);
        if (shadowClone) childNodes.push(shadowClone);
      }
    }

    const rect = sourceNode.getBoundingClientRect();
    const style = window.getComputedStyle(sourceNode);
    const area = (style.display === 'none' || style.visibility === 'hidden' || parseFloat(style.opacity) <= 0)?0:rect.width * rect.height;
    const isVisible = (rect.width > 1 && rect.height > 1 &&   
                  style.display !== 'none' && style.visibility !== 'hidden' &&   
                  parseFloat(style.opacity) > 0 &&  
                  Math.abs(rect.left) < 5000 && Math.abs(rect.top) < 5000) 
                  || isSmallDropdown;  
    const zIndex = style.position !== 'static' ? (parseInt(style.zIndex) || 0) : 0;
  
    let info = {
          rect, area, isVisible, isSmallDropdown, zIndex,
          style: {  
            display: style.display, visibility: style.visibility,  
            opacity: style.opacity, position: style.position
          }};
    
    const nonTextChildren = childNodes.filter(child => child.nodeType !== 3);  
    const hasValidChildren = nonTextChildren.length > 0;  
          
    if (hasValidChildren) {
      const childrenInfos = nonTextChildren.map(c => nodeInfo.get(c)).filter(i => i && i.rect && i.rect.width > 0 && i.rect.height > 0);
      const bgAlpha = (() => {
        const c = style.backgroundColor;
        if (!c || c === 'transparent') return 0;
        const m = c.match(/rgba?\([^)]+,\s*([\d.]+)\)/);
        return m ? parseFloat(m[1]) : 1;
      })();
      const hasVisualBg = bgAlpha > 0.1 || style.backgroundImage !== 'none' || (style.backdropFilter && style.backdropFilter !== 'none') || style.boxShadow !== 'none';
      
      if (!hasVisualBg && childrenInfos.length > 0) {
        // Skip fixed/absolute children when computing parent's merged rect (they're out of flow)
        const flowChildren = childrenInfos.filter(cInfo => cInfo.style && cInfo.style.position !== 'fixed' && cInfo.style.position !== 'absolute');
        if (flowChildren.length > 0) {
          let minL = Infinity, minT = Infinity, maxR = -Infinity, maxB = -Infinity;
          for (const cInfo of flowChildren) {
            minL = Math.min(minL, cInfo.rect.left);
            minT = Math.min(minT, cInfo.rect.top);
            maxR = Math.max(maxR, cInfo.rect.right);
            maxB = Math.max(maxB, cInfo.rect.bottom);
          }
          info.rect = { left: minL, top: minT, right: maxR, bottom: maxB, width: maxR - minL, height: maxB - minT };
          info.area = info.rect.width * info.rect.height;
        } else {
          const maxC = childrenInfos.filter(i => i.isVisible).sort((a, b) => b.area - a.area)[0];
          if (maxC && maxC.area > 10000 && (!isVisible || maxC.area > info.area * 5)) info = maxC;
        }
      }
    }

    if (sourceNode.nodeType === 1 && sourceNode.tagName === 'DIV') {    
      if (!hasValidChildren && !sourceNode.textContent.trim()) return null; 
    }  
    // aria-hidden + not visible = truly hidden (e.g. mobile menus), remove even if has children
    if (sourceNode.getAttribute && sourceNode.getAttribute('aria-hidden') === 'true' && !info.isVisible) {
      return null;
    }
    if (info.isVisible || hasValidChildren || keep) {  
      childNodes.forEach(child => clone.appendChild(child));  
      return clone;  
    }  
    return null;  
  }  
  
  return {  
    domCopy: cloneNode(document.body),  
    getNodeInfo: node => nodeInfo.get(node),  
    isVisible: node => {  
      const info = nodeInfo.get(node);  
      return info && info.isVisible;  
    }  
  };  
}  
const { domCopy, getNodeInfo, isVisible } = createEnhancedDOMCopy();
if (text_only) {
  const blocks = new Set(['DIV','P','H1','H2','H3','H4','H5','H6','LI','TR','SECTION','ARTICLE','HEADER','FOOTER','NAV','BLOCKQUOTE','PRE','HR','BR','DT','DD','FIGCAPTION','DETAILS','SUMMARY']);
  domCopy.querySelectorAll('*').forEach(el => {
    if (blocks.has(el.tagName)) el.insertAdjacentText('beforebegin', '\n');
  });
  domCopy.querySelectorAll('input:not([type=hidden]),textarea,select').forEach(el=>{
    const p=[el.tagName,el.id&&'#'+el.id,el.getAttribute('name')&&'name='+el.getAttribute('name'),el.tagName==='INPUT'&&'type='+(el.getAttribute('type')||'text'),el.getAttribute('placeholder')&&'"'+el.getAttribute('placeholder')+'"',el.getAttribute('data-autofilled')&&'autofilled',el.disabled&&'disabled',el.tagName==='SELECT'&&el.getAttribute('data-selected')&&'="'+el.getAttribute('data-selected')+'"'].filter(Boolean).join(' ');
    el.insertAdjacentText('beforebegin','\n['+p+']\n');
  });
  domCopy.querySelectorAll('button[disabled]').forEach(el=>el.insertAdjacentText('beforebegin','[DISABLED] '));
  return domCopy.textContent;
}
const viewportArea = window.innerWidth * window.innerHeight; 

function analyzeNode(node, pPathType='main') {  
    // 处理非元素节点和叶节点  
    if (node.nodeType !== 1 || !node.children.length) {  
      node.nodeType === 1 && (node.dataset.mark = 'K:leaf');  
      return;  
    }  
    const pathType = (node.dataset.mark === 'K:secondary') ? 'second' : pPathType;  
    const nodeInfoData = getNodeInfo(node);
    if (!nodeInfoData || !nodeInfoData.rect) return;
    const rectn = nodeInfoData.rect; 
    if (rectn.width < window.innerWidth * 0.8 && rectn.height < window.innerHeight * 0.8) return node;
    if (node.tagName === 'TABLE') return;
    const children = Array.from(node.children);  
    if (children.length === 1) {  
      node.dataset.mark = 'K:container';  
      return analyzeNode(children[0], pathType);  
    }  
    if (children.length > 10) return;
    
    // 获取子元素信息并排序  
    const childrenInfo = children.map(child => {  
      const info = getNodeInfo(child) || { rect: {}, style: {} };  
      return { node: child, rect: info.rect, style: info.style, 
          area: info.area, zIndex: (info.zIndex || 0), isVisible: info.isVisible };  
    });
    childrenInfo.sort((a, b) => b.area - a.area);  
    
    // 检测是划分还是覆盖  
    const isOverlay = hasOverlap(childrenInfo);  
    node.dataset.mark = isOverlay ? 'K:overlayParent' : 'K:partitionParent';  
    
    if (isOverlay) handleOverlayContainer(childrenInfo, pathType);  
    else handlePartitionContainer(childrenInfo, pathType);  

    console.log(`${isOverlay ? '覆盖' : '划分'}容器:`, node, `子元素数量: ${children.length}`);  
    console.log('子元素及标记:', children.map(child => ({   
      element: child,   
      mark: child.dataset.mark || '无',  
      info: getNodeInfo ? getNodeInfo(child) : undefined  
    })));  
    for (const child of children)  
      if (!child.dataset.mark || child.dataset.mark[0] !== 'R') analyzeNode(child, pathType);  
  }  
  
  // 处理划分容器  
  function handlePartitionContainer(childrenInfo, pathType) {  
    childrenInfo.sort((a, b) => b.area - a.area);
    const totalArea = childrenInfo.reduce((sum, item) => sum + item.area, 0);  
    console.log(childrenInfo[0].area / totalArea);
    const hasMainElement = childrenInfo.length >= 1 &&   
                          (childrenInfo[0].area / totalArea > 0.5) &&   
                          (childrenInfo.length === 1 || childrenInfo[0].area > childrenInfo[1].area * 2);  
    if (hasMainElement) {  
      childrenInfo[0].node.dataset.mark = 'K:main';
      for (let i = 1; i < childrenInfo.length; i++) {  
        const child = childrenInfo[i];  
        let className = (child.node.getAttribute('class') || '').toLowerCase();
        let isSecondary = containsButton(child.node);
        if (className.includes('nav')) isSecondary = true;
        if (className.includes('breadcrumbs')) isSecondary = true;
        if (className.includes('header') && className.includes('table')) isSecondary = true;
        if (child.node.innerHTML.trim().replace(/\s+/g, '').length < 500) isSecondary = true;
        if (child.node.textContent.trim().length > 200) isSecondary = true;  // P3: 有实质文本内容则保留
        if (child.style.visibility === 'hidden') isSecondary = false;
        if (isSecondary) child.node.dataset.mark = 'K:secondary';  
        else child.node.dataset.mark = 'K:nonEssential';  
      }  
    } else {  
      return; // relaxed: skip equalmany filtering, list truncation handles token budget
      const uniqueClassNames = new Set(childrenInfo.map(item => item.node.getAttribute('class') || '')).size;  
      const highClassNameVariety = uniqueClassNames >= childrenInfo.length * 0.8;  
      if (pathType !== 'main' && highClassNameVariety && childrenInfo.length > 5) {
        childrenInfo.forEach(child => child.node.dataset.mark = 'R:equalmany');  
      } else {
        childrenInfo.forEach(child => child.node.dataset.mark = 'K:equal');  
      }
    }  
  }  

  function containsButton(container) {  
    const hasStandardButton = container.querySelector('button, input[type="button"], input[type="submit"], [role="button"]') !== null;  
    if (hasStandardButton) return true;  
    const hasClassButton = container.querySelector('[class*="-btn"], [class*="-button"], .button, .btn, [class*="btn-"]') !== null;  
    return hasClassButton;  
  }   
  
  function handleOverlayContainer(childrenInfo, pathType) {  
    // elementFromPoint ground truth: 让浏览器告诉我们谁在视觉最上层
    const _efp = document.elementFromPoint(window.innerWidth/2, window.innerHeight/2);
    if (_efp) { let _el = _efp; while (_el) { const _h = childrenInfo.find(c => c.node.id && c.node.id === _el.id); if (_h) { _h.zIndex = 9999; break; } _el = _el.parentElement; } }
    const sorted = [...childrenInfo].sort((a, b) => b.zIndex - a.zIndex);  
    console.log('排序后的子元素:', sorted);
    if (sorted.length === 0) return;  
    
    const top = sorted[0];  
    const rect = top.rect;  
    const topNode = top.node; 
    const isComplex = top.node.querySelectorAll('input, select, textarea, button, a, [role="button"]').length >= 1;  

    const textContent = topNode.textContent?.trim() || '';  
    const textLength = textContent.length;  
    const hasLinks = topNode.querySelectorAll('a').length > 0;  
    const isMostlyText = textLength > 7 && !hasLinks;  

    const centerDiff = Math.abs((rect.left + rect.width/2) - window.innerWidth/2) / window.innerWidth;  
    const minDimensionRatio = Math.min(rect.width / window.innerWidth, rect.height / window.innerHeight);  
    const maxDimensionRatio = Math.max(rect.width / window.innerWidth, rect.height / window.innerHeight);  
    const isNearTop = rect.top < 50;  
    const isDialog = (top.node.querySelector('iframe') || top.node.querySelector('button') || top.node.querySelector('input')) && centerDiff < 0.3;

    if (isComplex && centerDiff < 0.2 && 
        ((minDimensionRatio > 0.2 && rect.width/window.innerWidth < 0.98) || minDimensionRatio > 0.95)) {  
      top.node.dataset.mark = 'K:mainInteractive';  
       sorted.slice(1).forEach(e => {
          if ((parseInt(e.zIndex)||0) <= (parseInt(sorted[0].zIndex)||0)) {
              e.node.dataset.mark = 'R:covered';
          } else {
              e.node.dataset.mark = 'K:noncovered';
          }
      });
    } else {
      if (isComplex && isNearTop && maxDimensionRatio > 0.4 && top.isVisible) {
        top.node.dataset.mark = 'K:topBar';
      } else if (isMostlyText || isComplex || isDialog) {  
        topNode.dataset.mark = 'K:messageContent'; 
      } else {  
        topNode.dataset.mark = 'R:floatingAd'; 
      }  
      const rest = sorted.slice(1);  
      rest.length && (!hasOverlap(rest) ? handlePartitionContainer(rest, pathType) : handleOverlayContainer(rest, pathType));  
    } 
  }  
    
  function hasOverlap(items) {  
    return items.some((a, i) =>   
      items.slice(i+1).some(b => {  
        const r1 = a.rect, r2 = b.rect;  
        if (!r1.width || !r2.width || !r1.height || !r2.height) {return false;}
        const epsilon = 1;
        const x1 = r1.x !== undefined ? r1.x : r1.left;
        const y1 = r1.y !== undefined ? r1.y : r1.top;
        const x2 = r2.x !== undefined ? r2.x : r2.left;
        const y2 = r2.y !== undefined ? r2.y : r2.top;
        return !(x1 + r1.width <= x2 + epsilon || x1 >= x2 + r2.width - epsilon || 
            y1 + r1.height <= y2 + epsilon || y1 >= y2 + r2.height - epsilon
        );
      })
    );  
}

// Hoist top 1-2 deep fixed dialogs to body level for overlay detection
const _fc = [...domCopy.querySelectorAll('*')].filter(el => {
  if (el.parentNode === domCopy) return false;
  const info = getNodeInfo(el);
  if (!info?.rect || info.style.position !== 'fixed') return false;
  const r = info.rect, cover = (r.width * r.height) / viewportArea;
  const cd = Math.abs((r.left + r.width/2) - window.innerWidth/2) / window.innerWidth;
  return cover > 0.15 && cover < 1.0 && cd < 0.3 && el.querySelector('button, input, a, [role="button"], iframe');
}).filter((el, _, arr) => !arr.some(o => o !== el && o.contains(el)))
  .sort((a, b) => (getNodeInfo(b).rect.width * getNodeInfo(b).rect.height) - (getNodeInfo(a).rect.width * getNodeInfo(a).rect.height))
  .slice(0, 2);
_fc.forEach(el => { const r = getNodeInfo(el).rect; console.log('[simphtml] Hoisted fixed dialog:', el.tagName + (el.id ? '#'+el.id : '') + (el.className ? '.'+String(el.className).split(' ')[0] : ''), Math.round(r.width)+'x'+Math.round(r.height), Math.round(100*r.width*r.height/viewportArea)+'%'); el.parentNode.removeChild(el); domCopy.appendChild(el); });
const result = analyzeNode(domCopy); 
domCopy.querySelectorAll('[data-mark^="R:"]').forEach(el=>el.parentNode?.removeChild(el));  
let root = domCopy;  
while (root.children.length === 1) {  
  root = root.children[0];  
}  
for (let ii = 0; ii < 3; ii++) {
  root.querySelectorAll('div').forEach(div => (!div.textContent.trim() && div.children.length === 0) && div.remove());
}
root.querySelectorAll('[data-mark]').forEach(e => e.removeAttribute('data-mark'));  
root.removeAttribute('data-mark');
root.querySelectorAll('iframe').forEach(f => {
  if (f.children.length) {
    const d = document.createElement('div');
    for (const a of f.attributes) d.setAttribute(a.name, a.value);
    d.setAttribute('data-tag', 'iframe');
    while (f.firstChild) d.appendChild(f.firstChild);
    f.parentNode.replaceChild(d, f);
  }
});
return root.outerHTML;
    }
optHTML()
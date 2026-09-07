// A low-resolution WebGL mesh with a slow, frame-capped drift.
// The CSS gradient remains visible when WebGL isn't available.
const canvas = document.querySelector('#mesh');
const motion = matchMedia('(prefers-reduced-motion: reduce)');
const gl = canvas.getContext('webgl', { alpha: false, antialias: false, depth: false, powerPreference: 'low-power' });
if (gl) {
  const vertex = `attribute vec2 position; varying vec2 uv; void main(){ uv=position*.5+.5; gl_Position=vec4(position,0.,1.); }`;
  const originalFragment = `precision mediump float;
    varying vec2 uv;
    uniform vec2 pointer;
    uniform float time;
    float field(vec2 p, vec2 center, vec2 radius) {
      vec2 d=(p-center)/radius;
      return exp(-dot(d,d)*2.0);
    }
    void main(){
      vec2 p=vec2(uv.x,1.-uv.y);
      p += vec2(sin(p.y*5.8+p.x*2.+time*.09), cos(p.x*5.3-p.y*2.+time*.07))*0.065;
      p += pointer*0.035;
      vec3 base=vec3(.098,.090,.141);
      vec3 rose=vec3(.58,.27,.40);
      vec3 iris=vec3(.31,.22,.48);
      vec3 pine=vec3(.10,.33,.40);
      float a=field(p,vec2(.12+sin(time*.11)*.025,.65+sin(time*.08)*.018),vec2(.52,.46));
      float b=field(p,vec2(.62+sin(time*.07)*.025,.50+sin(time*.10)*.02),vec2(.57,.38));
      float c=field(p,vec2(.98+sin(time*.09)*.02,.77+sin(time*.06)*.025),vec2(.44,.47));
      vec3 color=base+rose*a*.64+iris*b*.58+pine*c*.66;
      float grain=fract(sin(dot(gl_FragCoord.xy,vec2(12.9898,78.233)))*43758.5453)-.5;
      color+=grain*.007;
      gl_FragColor=vec4(color,1.);
    }`;
  // Keep the previous treatment available at ?mesh=original for comparison.
  const sculptedFragment = `
    #ifdef GL_FRAGMENT_PRECISION_HIGH
    precision highp float;
    #else
    precision mediump float;
    #endif
    varying vec2 uv;
    uniform vec2 pointer;
    uniform vec3 windowBase;
    uniform float time;
    float field(vec2 p, vec2 center, vec2 radius) {
      vec2 d=(p-center)/radius;
      return exp(-dot(d,d)*2.0);
    }
    void main(){
      vec2 screen=vec2(uv.x,1.-uv.y);
      vec2 p=screen+pointer*.018;
      float t=time*.065;
      // Broad, nested waves bend the color fields into soft folds.
      p += .055*vec2(
        sin(p.y*6.5+sin(p.x*4.0+t)*.8-t),
        cos(p.x*5.0+sin(p.y*5.5-t)*.7+t*.8)
      );
      vec3 base=vec3(.098,.090,.141);
      vec3 rose=vec3(.58,.27,.40);
      vec3 iris=vec3(.31,.22,.48);
      vec3 pine=vec3(.10,.33,.40);
      float a=field(p,vec2(.08+sin(t)*.025,.62),vec2(.52,.43));
      float b=field(p,vec2(.62,.46+cos(t)*.025),vec2(.58,.38));
      float c=field(p,vec2(.98,.73+sin(t*.8)*.025),vec2(.46,.46));
      vec3 color=base+rose*a*.70+iris*b*.54+pine*c*.72;
      float wave=sin(p.x*7.0+p.y*3.0+sin(p.y*6.0-t)*.8+t*.6);
      float fold=exp(-pow(wave-.25,2.0)*10.0);
      float envelope=field(p,vec2(.5,.64),vec2(.85,.38));
      color += mix(rose,pine,smoothstep(.15,.85,p.x))*fold*envelope*.20;
      color *= 1.0-(1.0-fold)*envelope*.09;
      // A diffuse reflection follows the actual window's lower edge on resize.
      vec2 edge=(screen-windowBase.xy)/vec2(max(windowBase.z,.1),.065);
      float glow=exp(-edge.y*edge.y*2.0-pow(edge.x,4.0)*2.0);
      color += mix(vec3(.65,.38,.43),vec3(.32,.53,.58),screen.x)*glow*.16;
      // Stationary grain: texture without flicker or another rendering pass.
      float grain=fract(52.9829189*fract(dot(gl_FragCoord.xy,vec2(.06711056,.00583715))))-.5;
      color += grain*.018;
      gl_FragColor=vec4(color,1.);
    }`;
  const fragment = new URLSearchParams(location.search).get('mesh') === 'original' ? originalFragment : sculptedFragment;
  const compile = (type, source) => {
    const shader = gl.createShader(type);
    gl.shaderSource(shader, source); gl.compileShader(shader);
    return shader;
  };
  const vs = compile(gl.VERTEX_SHADER, vertex), fs = compile(gl.FRAGMENT_SHADER, fragment);
  if (vs && fs) {
    const program = gl.createProgram(); gl.attachShader(program, vs); gl.attachShader(program, fs); gl.linkProgram(program);
    const parallel = gl.getExtension('KHR_parallel_shader_compile');
    const initialize = () => {
      if (gl.isContextLost()) return;
      if (parallel && !gl.getProgramParameter(program, parallel.COMPLETION_STATUS_KHR)) {
        requestAnimationFrame(initialize);
        return;
      }
      if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
        gl.deleteProgram(program); gl.deleteShader(vs); gl.deleteShader(fs);
        return;
      }
      gl.useProgram(program);
      const buffer = gl.createBuffer(); gl.bindBuffer(gl.ARRAY_BUFFER, buffer);
      gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1,-1,1,-1,-1,1,-1,1,1,-1,1,1]), gl.STATIC_DRAW);
      const position = gl.getAttribLocation(program, 'position');
      gl.enableVertexAttribArray(position); gl.vertexAttribPointer(position,2,gl.FLOAT,false,0,0);
      const pointer = gl.getUniformLocation(program, 'pointer');
      const time = gl.getUniformLocation(program, 'time');
      const windowBase = gl.getUniformLocation(program, 'windowBase');
      const appWindow = document.querySelector('#demo-window');
      const control = document.querySelector('#background-motion');
      const product = document.querySelector('.product');
      let x = 0, y = 0, elapsed = 0, previous = 0;
      let frame = 0, timer = 0, paused = false, inView = true;
      let width = 1, height = 1;
      const canAnimate = () => !paused && !motion.matches && !document.hidden && inView && !gl.isContextLost();
      const stop = () => {
        cancelAnimationFrame(frame); clearTimeout(timer);
        frame = 0; timer = 0; previous = 0;
      };
      const draw = now => {
        frame = 0;
        if (document.hidden || gl.isContextLost()) { previous = 0; return; }
        const moving = canAnimate();
        if (moving && previous) elapsed += Math.min((now - previous) / 1000, .1);
        previous = moving ? now : 0;
        if (canvas.width !== width || canvas.height !== height) {
          canvas.width = width; canvas.height = height; gl.viewport(0, 0, width, height);
        }
        gl.uniform2f(pointer, motion.matches ? 0 : x, motion.matches ? 0 : y);
        gl.uniform1f(time, elapsed);
        gl.drawArrays(gl.TRIANGLES, 0, 6);
        canvas.classList.add('ready');
        if (moving) timer = setTimeout(() => { timer = 0; frame = requestAnimationFrame(draw); }, 1000 / 24);
      };
      const schedule = () => { if (!frame && !timer && !document.hidden) frame = requestAnimationFrame(draw); };
      const resize = () => {
        const bounds = canvas.getBoundingClientRect();
        const windowBounds = appWindow.getBoundingClientRect();
        gl.uniform3f(windowBase,
          (windowBounds.left + windowBounds.width / 2 - bounds.left) / Math.max(1, bounds.width),
          (windowBounds.bottom - bounds.top) / Math.max(1, bounds.height),
          windowBounds.width / 2 / Math.max(1, bounds.width));
        width = Math.max(1, Math.min(960, Math.round(bounds.width * .65)));
        height = Math.max(1, Math.round(width * bounds.height / Math.max(1, bounds.width)));
        schedule();
      };
      const update = () => {
        stop();
        control.hidden = motion.matches || gl.isContextLost();
        control.setAttribute('aria-pressed', String(paused));
        const label = paused ? 'Resume background animation' : 'Pause background animation';
        control.setAttribute('aria-label', label); control.title = label;
        schedule();
      };
      control.addEventListener('click', () => { paused = !paused; update(); });
      document.addEventListener('visibilitychange', update);
      motion.addEventListener('change', update);
      new ResizeObserver(resize).observe(document.body);
      new IntersectionObserver(entries => {
        inView = entries[0].isIntersecting;
        stop(); schedule();
      }, { rootMargin: '120px' }).observe(product);
      product.addEventListener('pointermove', event => {
        if (motion.matches || paused || event.pointerType === 'touch') return;
        x = event.clientX / innerWidth - .5; y = event.clientY / innerHeight - .5;
        schedule();
      }, { passive: true });
      canvas.addEventListener('webglcontextlost', () => {
        stop(); canvas.classList.remove('ready'); control.hidden = true;
      });
      resize(); update();
    };
    requestAnimationFrame(initialize);
  }
}

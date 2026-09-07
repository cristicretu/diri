// A low-resolution WebGL mesh with a slow, frame-capped drift.
// The CSS gradient remains visible when WebGL isn't available.
const canvas = document.querySelector('#mesh');
const motion = matchMedia('(prefers-reduced-motion: reduce)');
const gl = canvas.getContext('webgl', { alpha: false, antialias: false, depth: false, powerPreference: 'low-power' });
if (gl) {
  const vertex = `attribute vec2 position; varying vec2 uv; void main(){ uv=position*.5+.5; gl_Position=vec4(position,0.,1.); }`;
  const fragment = `precision mediump float;
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
  const compile = (type, source) => {
    const shader = gl.createShader(type);
    gl.shaderSource(shader, source); gl.compileShader(shader);
    if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) { gl.deleteShader(shader); return null; }
    return shader;
  };
  const vs = compile(gl.VERTEX_SHADER, vertex), fs = compile(gl.FRAGMENT_SHADER, fragment);
  if (vs && fs) {
    const program = gl.createProgram(); gl.attachShader(program, vs); gl.attachShader(program, fs); gl.linkProgram(program);
    if (gl.getProgramParameter(program, gl.LINK_STATUS)) {
      gl.useProgram(program);
      const buffer = gl.createBuffer(); gl.bindBuffer(gl.ARRAY_BUFFER, buffer);
      gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1,-1,1,-1,-1,1,-1,1,1,-1,1,1]), gl.STATIC_DRAW);
      const position = gl.getAttribLocation(program, 'position');
      gl.enableVertexAttribArray(position); gl.vertexAttribPointer(position,2,gl.FLOAT,false,0,0);
      const pointer = gl.getUniformLocation(program, 'pointer');
      const time = gl.getUniformLocation(program, 'time');
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
    }
  }
}

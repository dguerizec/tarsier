#version 330
out vec4 color;
uniform vec2 resolution;
uniform float time;
uniform float relief;
uniform int palette;
float hash(vec2 p){return fract(sin(dot(p,vec2(127.1,311.7)))*43758.5453);}
float noise(vec2 p){vec2 i=floor(p),f=fract(p);f=f*f*(3.-2.*f);return mix(mix(hash(i),hash(i+vec2(1,0)),f.x),mix(hash(i+vec2(0,1)),hash(i+1.),f.x),f.y);}
float terrain(vec2 p){return relief*(.65*sin(p.x*.32)+.6*sin(p.y*.27+p.x*.16)+.26*sin(p.x*.8+p.y*.45)+.18*noise(p*.8));}
float smin(float a,float b,float k){float h=max(k-abs(a-b),0.)/k;return min(a,b)-h*h*k*.25;}
// Thin tapered blades, twisted and bent by a shared current.
float blade(vec3 p,float height,float phase,float lean){
 float v=clamp(p.y/height,0.,1.);
 p.x-=lean*v*v+.27*sin(v*4.5-time*.8+phase)*v;
 p.z-=.25*sin(v*5.-time*.65+phase)*v;
 float angle=.6*sin(v*4.+phase+time*.3);
 p.xz=mat2(cos(angle),-sin(angle),sin(angle),cos(angle))*p.xz;
 float width=.025+.28*pow(max(sin(v*3.141593),0.),.8)*(1.-.35*v);
 // Fine edge ripples preserve a broad, flat lamina.
 p.z+=.04*sin(v*24.+phase+time*.6)*pow(clamp(abs(p.x)/max(width,.025),0.,1.),2.);
 vec3 d=abs(vec3(p.x,p.y-height*.5,p.z))-vec3(width,height*.5,.026);
 return (length(max(d,0.))+min(max(d.x,max(d.y,d.z)),0.))*.55;
}
vec2 scene(vec3 p){
 float ground=(p.y-terrain(p.xz))*.65;
 vec2 cell=floor((p.xz+3.)/6.);
 vec2 c=cell*6.;
 float seed=hash(cell);
 c+=vec2(sin(seed*40.),cos(seed*27.))*.55;
 vec3 q=p-vec3(c.x,terrain(c),c.y);
 float height=(3.4+seed*2.)*relief;
 float kelp=blade(q,height,seed*20.,.4);
 kelp=min(kelp,blade(q-vec3(.28,0,.15),height*.79,seed*20.+1.7,1.));
 kelp=min(kelp,blade(q-vec3(-.26,0,-.12),height*.87,seed*20.+3.4,-.85));
 return vec2(min(ground,kelp),kelp<ground?1.:0.);
}
vec3 normal(vec3 p){vec2 e=vec2(.003,-.003);return normalize(e.xyy*scene(p+e.xyy).x+e.yyx*scene(p+e.yyx).x+e.yxy*scene(p+e.yxy).x+e.xxx*scene(p+e.xxx).x);}
float shadow(vec3 p,vec3 l){float result=1.,t=.07;for(int i=0;i<18;i++){float h=scene(p+l*t).x;result=min(result,10.*h/t);t+=clamp(h,.08,.65);if(h<.001)break;}return clamp(result,.15,1.);}
void main(){
 vec2 uv=(2.*gl_FragCoord.xy-resolution)/resolution.y;
 vec3 ro=vec3(2.7+.4*sin(time*.065),3.4,9.5);
 vec3 target=vec3(.3,1.1,-5.);
 vec3 f=normalize(target-ro),r=normalize(cross(f,vec3(0,1,0))),u=cross(r,f);
 vec3 rd=normalize(f*1.65+r*uv.x+u*uv.y);
 vec3 skyLow=vec3(.37,.61,.54),skyHigh=vec3(.09,.30,.31),plant=vec3(.36,.48,.12),grass=vec3(.27,.34,.22);
 if(palette==1){skyLow=vec3(.34,.56,.63);skyHigh=vec3(.055,.12,.23);plant=vec3(.16,.53,.49);grass=vec3(.09,.23,.29);}
 if(palette==2){skyLow=vec3(.57,.64,.58);skyHigh=vec3(.18,.36,.39);plant=vec3(.58,.32,.12);grass=vec3(.33,.35,.27);}
 vec3 light=normalize(vec3(-.6,.85,.4));
 vec3 sky=mix(skyLow,skyHigh,clamp(rd.y*.8+.15,0.,1.));
 sky+=vec3(1.,.73,.41)*pow(max(dot(rd,light),0.),48.)*.32;
 float t=0.;vec2 hit=vec2(1,0);bool found=false;
 for(int i=0;i<160;i++){hit=scene(ro+rd*t);if(hit.x<.0025*(1.+t*.03)){found=true;break;}t+=max(hit.x*.8,.003);if(t>55.)break;}
 vec3 col=sky;
 if(found){vec3 p=ro+rd*t,n=normal(p);
 float ao=1.;for(int i=1;i<=3;i++){float d=float(i)*.18;ao-=(d-scene(p+n*d).x)*(.32/float(i));}ao=clamp(ao,.3,1.);
 vec3 base=mix(grass,plant,hit.y);
 float strata=.5+.5*sin(p.y*2.+noise(p.xz*1.5)*3.);
 base*=.9+.12*strata;
 float sun=max(dot(n,light),0.)*shadow(p+n*.02,light);
 col=base*(vec3(.38,.46,.49)*(.6+.4*n.y)*ao+vec3(1.35,1.12,.84)*sun);
 col+=plant*hit.y*.25*max(dot(-n,light),0.);
 col+=vec3(.10,.065,.035)*pow(1.-max(dot(n,-rd),0.),3.);
 float fog=1.-exp(-t*.036);col=mix(col,skyLow,fog);
 }
 col=pow(max(col,0.),vec3(.88));
 col*=1.-.1*dot(uv*.45,uv*.45);
 color=vec4(col,1.);
}

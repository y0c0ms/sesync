import base64,hmac,hashlib,struct,time,sys
s=open('/var/tmp/sesync-demo/team/totp.secret').read().strip()
k=base64.b32decode(s+'='*(-len(s)%8))
c=int(time.time())//30
h=hmac.new(k,struct.pack('>Q',c),hashlib.sha1).digest()
o=h[-1]&15
print('%06d'%((struct.unpack('>I',h[o:o+4])[0]&0x7fffffff)%1000000))

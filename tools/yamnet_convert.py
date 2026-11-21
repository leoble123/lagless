"""Convert official YAMNet Keras weights (yamnet.h5) to lagless's compact blob.

Batch-norm (center, no scale, eps 1e-4) is folded into each conv: w' = w / sqrt(var+eps),
b = beta - mean / sqrt(var+eps). Weights are stored as float16, biases as float32.
Layout per layer (little-endian):
  layer1 conv:  w[32][3][3] (in=1),        b[32]
  depthwise:    w[C][3][3],                b[C]
  pointwise:    w[Cout][Cin],              b[Cout]
  dense:        w[521][1024],              b[521]
"""
import sys, struct, h5py, numpy as np
src, dst = sys.argv[1], sys.argv[2]
f = h5py.File(src, 'r')
EPS = 1e-4
def g(path): return np.array(f[path])
def bn(prefix):
    beta = g(f'{prefix}/{prefix}/beta:0'); mean = g(f'{prefix}/{prefix}/moving_mean:0'); var = g(f'{prefix}/{prefix}/moving_variance:0')
    s = 1.0 / np.sqrt(var + EPS)
    return s, beta - mean * s
out = bytearray(b'YAMN\x01\x00\x00\x00')
def put(w, b):
    out.extend(np.asarray(w, '<f2').tobytes()); out.extend(np.asarray(b, '<f4').tobytes())
# layer1: kernel (3,3,1,32)
k = g('layer1/conv/layer1/conv/kernel:0'); s, b = bn('layer1/conv/bn')
put((k[:, :, 0, :] * s).transpose(2, 0, 1), b)            # [32][ky][kx]
for i in range(2, 15):
    L = f'layer{i}'
    k = g(f'{L}/depthwise_conv/{L}/depthwise_conv/depthwise_kernel:0')  # (3,3,C,1)
    s, b = bn(f'{L}/depthwise_conv/bn')
    put((k[:, :, :, 0] * s).transpose(2, 0, 1), b)        # [C][ky][kx]
    k = g(f'{L}/pointwise_conv/{L}/pointwise_conv/kernel:0')            # (1,1,Cin,Cout)
    s, b = bn(f'{L}/pointwise_conv/bn')
    put((k[0, 0] * s).T, b)                                # [Cout][Cin]
names = []
f.visit(lambda n: names.append(n) if n.endswith('kernel:0') and 'logits' in n else None)
kd = g([n for n in names][0]); bd = g([n for n in names][0].replace('kernel', 'bias'))
put(kd.T, bd)                                              # [521][1024]
open(dst, 'wb').write(out)
print('wrote', len(out), 'bytes; dense', kd.shape)

"""Reference YAMNet in TensorFlow, written from the published architecture (tensorflow/models
research/audioset/yamnet: params.py, features.py, yamnet.py) and loading the official yamnet.h5
weights. Saves each clip's 16 kHz input and the reference scores for comparison with lagless."""
import sys, os, glob, numpy as np, soundfile as sf
import h5py
import tensorflow as tf
from tensorflow.keras import layers, Model
from scipy.signal import resample_poly

weights, clip_dir, out_dir = sys.argv[1:4]
SR, MEL, EPS = 16000, 64, 1e-4
DEFS = [(None, 2, 32), ('s', 1, 64), ('s', 2, 128), ('s', 1, 128), ('s', 2, 256), ('s', 1, 256), ('s', 2, 512),
        ('s', 1, 512), ('s', 1, 512), ('s', 1, 512), ('s', 1, 512), ('s', 1, 512), ('s', 2, 1024), ('s', 1, 1024)]

def bn(name):
    return layers.BatchNormalization(name=name, center=True, scale=False, epsilon=EPS)

ASSIGN = []  # (keras layer, official h5 weight paths in order)

def net(features):
    x = layers.Reshape((96, 64, 1))(features)
    for i, (kind, stride, filters) in enumerate(DEFS):
        n = f'layer{i + 1}'
        parts = [('conv', layers.Conv2D(filters, 3, stride, 'same', use_bias=False), 'kernel')] if kind is None else [
            ('depthwise_conv', layers.DepthwiseConv2D(3, stride, 'same', use_bias=False), 'depthwise_kernel'),
            ('pointwise_conv', layers.Conv2D(filters, 1, 1, 'same', use_bias=False), 'kernel')]
        for part, conv, kname in parts:
            norm = layers.BatchNormalization(center=True, scale=False, epsilon=EPS)
            x = layers.ReLU()(norm(conv(x)))
            ASSIGN.append((conv, [f'{n}/{part}/{n}/{part}/{kname}:0']))
            b = f'{n}/{part}/bn/{n}/{part}/bn'
            ASSIGN.append((norm, [f'{b}/beta:0', f'{b}/moving_mean:0', f'{b}/moving_variance:0']))
    x = layers.GlobalAveragePooling2D()(x)
    dense = layers.Dense(521)
    ASSIGN.append((dense, ['logits/logits/kernel:0', 'logits/logits/bias:0']))
    return layers.Activation('sigmoid')(dense(x))

def log_mel_patches(wave):
    spec = tf.abs(tf.signal.stft(wave, 400, 160, 512))
    m = tf.signal.linear_to_mel_weight_matrix(MEL, 257, SR, 125.0, 7500.0)
    logmel = tf.math.log(tf.matmul(spec, m) + 0.001)
    return logmel, tf.signal.frame(logmel, 96, 48, axis=0)

inp = layers.Input((96, 64))
model = Model(inp, net(inp))
with h5py.File(weights, 'r') as f:
    for layer, paths in ASSIGN:
        layer.set_weights([np.array(f[q]) for q in paths])

os.makedirs(out_dir, exist_ok=True)
for path in sorted(glob.glob(os.path.join(clip_dir, '*.wav'))):
    x, sr = sf.read(path, dtype='float32')
    if x.ndim > 1:
        x = x.mean(1)
    if sr != SR:
        x = resample_poly(x, 160, sr // 100).astype(np.float32)
    logmel, patches = log_mel_patches(tf.constant(x))
    scores = model(patches).numpy()
    base = os.path.basename(path)[:-4]
    x.astype('<f4').tofile(os.path.join(out_dir, base + '.pcm'))
    scores.astype('<f4').tofile(os.path.join(out_dir, base + '.scores'))
    logmel.numpy().astype('<f4').tofile(os.path.join(out_dir, base + '.mel'))
    print(base, scores.shape[0], ' '.join(str(i) for i in np.argsort(-scores.max(0))[:3]),
          ' '.join(f'{v:.2f}' for v in np.sort(scores.max(0))[::-1][:3]))

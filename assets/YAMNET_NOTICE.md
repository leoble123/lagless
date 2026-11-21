# YAMNet

`yamnet.bin` is derived from the official YAMNet weights (`yamnet.h5`, published by Google at
storage.googleapis.com/audioset/yamnet.h5) and `yamnet_class_map.csv` is the official class map,
both from https://github.com/tensorflow/models/tree/master/research/audioset/yamnet.

They are licensed under the Apache License 2.0 (see `YAMNET_LICENSE.txt`).

Changes: batch-normalisation folded into the convolution weights and the result stored as
float16, by `tools/yamnet_convert.py`. `tools/yamnet_reference.py` rebuilds the model in
TensorFlow from the published architecture to check lagless's Rust port against it.
